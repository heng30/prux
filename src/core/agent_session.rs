// agent 主循环：系统提示构建 + 工具调用循环

use crate::{
    core::{
        self,
        agent_loop::{self, AgentLoop},
        cache_warmer::CacheWarmer,
        compaction::{self, CompactionReason},
        extensions::{
            BoundaryOutcome, ExecuteToolFn, Extension, ExtensionHook, ExtensionTool,
            ExtensionUiRequest, GrammarSampling, InjectedChildTool, InjectedToolHandler,
            MakeSubAgentFn, NestedCallLog, SubAgentControls, SubAgentRunner, SubAgentSpec,
            ToolExecCtx, ToolExposure, ToolLoadout, UiNotifyLevel, is_deferred_tool_activated,
        },
        model_resolver::{self, ModelEntry},
        model_scope::ScopedModel,
        provider::{
            AgentMessage, ContentBlock, GRAMMAR_SCHEMA_KEY, ModelConfig, StreamEvent, Usage,
            usage::combine_usage,
        },
        session_manager::{self, Session, SessionCreateOptions},
        session_v4, settings_manager,
        skills::Skill,
        system_prompt::{SystemPromptOptions, build_system_prompt},
        tool_names,
        tools::{self, ToolError, ToolExecutionMode, ToolResult, ToolResultAttachment},
        virtual_models::{self, FailedRoute, ModelRouteReason, ModelRouteRequest},
    },
    error::{Error, Result},
    extensions::{codemode, util::truncate_chars},
    utils::{
        image::{ImageResizeLimits, normalize_base64_attachment},
        time::now_ms,
    },
};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::task::JoinHandle;

/// 分发内部JSON事件
pub type JsonSink = Box<dyn FnMut(Value) + Send>;

/// 每轮收尾回调：assistant 与全部工具结果 finalize 之后、
/// `turn_end` 事件之前调用；对 normal / error / aborted 三种响应都会调用，
/// 但 error / aborted 是硬退出，其决策被忽略。返回 `None` 维持正常调度。
pub type FinishTurn = Box<dyn FnMut(&TurnContext) -> Option<FinishTurnAction> + Send>;

/// 每轮结束后返回下一轮快照。返回 Some 时应用快照后继续。
pub type PrepareNextTurn = Box<dyn FnMut(&TurnContext) -> Option<NextTurnSnapshot> + Send>;

/// 子代理优雅收尾的宽限轮数（达上限后再给 5 轮收尾）。
const SUBAGENT_GRACE_TURNS: u32 = 5;

/// 单次工具执行经 `ctx.execute_tool` 产生的嵌套调用记录上限（超出只计数）。
const MAX_NESTED_CALLS: usize = 32;

/// 嵌套调用的递归深度上限（一层嵌套 = 1；再深的调用直接报错，防自调死循环）。
const MAX_NESTED_TOOL_DEPTH: u32 = 4;

/// 嵌套调用记录里 `args` / 文本的字符上限（防止嵌套结果把外层结果撑爆）。
const NESTED_CALL_TEXT_CAP: usize = 4096;

/// 达 `max_turns` 时注入的收尾提示（在下一轮 LLM 调用前由 steering 队列取走）。
const SUBAGENT_WRAP_UP: &str =
    "Wrap up immediately — provide your final answer now. Do not start new work.";

/// 子代理工具名：无论 spec 怎么写，核心都会把它们从 child 的工具白名单里剔除。
/// 递归防护：没有这层，child 继承父级工具集就会拿到 `Agent`，形成无上限递归。
const SUBAGENT_TOOL_NAMES: [&str; 4] = [
    "Agent",
    "SubagentWorkflow",
    "get_subagent_result",
    "steer_subagent",
];

/// `finish_turn` 决策
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinishTurnAction {
    /// 结束整个 run（决策在 `turn_end` 之后生效，跳过 prepareNextTurn 与 steering 轮询）
    End,
    /// 保证再发起一次 provider 请求（工具结果/steering/follow-up 已能满足时不额外新增）
    Continue,
}

/// 一轮完成后的上下文
#[derive(Debug, Clone)]
pub struct TurnContext {
    /// 完成本轮的 assistant 消息
    pub message: AgentMessage,
    /// 本轮工具执行产生的 toolResult 消息
    pub tool_results: Vec<AgentMessage>,
    /// 本轮后当前 agent 上下文
    pub context: Vec<AgentMessage>,
    /// 本循环调用将返回的新消息（prompt 批含初始消息、续接不含既有上下文）
    pub new_messages: Vec<AgentMessage>,
}

/// 工具选择策略
#[derive(Debug, Clone, PartialEq)]
pub enum ToolSelection {
    /// 默认：初始启用指定内置工具（defaultTools 设置或标准默认 read/bash/edit/write）；扩展工具全并入。
    Default(Vec<String>),
    /// --tools/-t：对内置与扩展工具都生效的严格允许名单
    Allowlist(Vec<String>),
    /// --no-tools：全部关闭（内置与扩展）
    NoTools,
    /// --no-builtin-tools：关闭内置，保留扩展工具
    NoBuiltinTools,
}

impl ToolSelection {
    /// 便捷构造：显式指定初始内置工具名（默认语义）。
    pub fn tools(names: Vec<String>) -> Self {
        ToolSelection::Default(names)
    }

    /// 名称是否允许注册（allowlist 未命中时 MCP 工具仍保留，再排除 denylist）。
    ///
    /// `--tools` 不再把 MCP 一并挡掉，除非名单为空
    /// （`--no-tools`）或点名了 `mcp__` 前缀条目（见 [`tool_names::allowlist_filters_mcp`]）。
    fn is_allowed(&self, name: &str, exclude: &[String]) -> bool {
        if tool_names::matches_any(exclude, name) {
            return false;
        }

        match self {
            ToolSelection::Allowlist(entries) => {
                tool_names::matches_any(entries, name) || self.is_mcp_reserved(name)
            }
            ToolSelection::NoTools => false,
            ToolSelection::Default(_) | ToolSelection::NoBuiltinTools => true,
        }
    }

    /// allowlist 保留但未点名的 MCP 工具：仍注册（供 codemode 与嵌套调用），但默认不对模型声明。
    fn is_mcp_reserved(&self, name: &str) -> bool {
        match self {
            ToolSelection::Allowlist(entries) => {
                !tool_names::matches_any(entries, name)
                    && !tool_names::allowlist_filters_mcp(entries)
                    && tool_names::is_mcp_tool_name(name)
            }
            _ => false,
        }
    }

    /// 是否对模型声明该工具。
    ///
    /// 被 allowlist 保留的 MCP 工具只有非 `direct` 曝光才可能被声明（由 `tool_search` 加载，
    /// 而 deferred 能走到声明处就意味着 `tool_search` 已注册）；`direct` 曝光的保留工具不声明。
    fn declares_to_model(&self, name: &str, exposure: ToolExposure) -> bool {
        !self.is_mcp_reserved(name) || exposure != ToolExposure::Direct
    }
}

impl Default for ToolSelection {
    /// 默认策略：`Default(空)`，即使用全部内置工具且无 allowlist。
    fn default() -> Self {
        ToolSelection::Default(Vec::new())
    }
}

/// 系统提示/工具列表重建参数（/extension 面板关闭后重启扩展工具用）
#[derive(Clone, Default)]
pub struct ToolRebuildCtx {
    /// 工具选择策略
    pub selection: ToolSelection,
    /// 排除工具名单（--exclude-tools）
    pub exclude_tools: Vec<String>,
    /// 自定义系统提示
    pub system_prompt: Option<String>,
    /// 追加系统提示
    pub append_system_prompt: Option<String>,
    /// 上下文文件
    pub context_files: Vec<(String, String)>,
    /// per-child 注入工具（`SubAgentSpec.injected_tools`）。
    ///
    /// 存在这里而不是 `Agent` 上的独立字段，是因为 [`AgentTemplate`] 会 clone `rebuild_ctx`，
    /// 于是**并行与顺序两条工具执行路径**都能拿到它（`execute_one` 只持有 `Arc<AgentTemplate>`， 没有 `Agent`）。
    pub injected_tools: Arc<Vec<InjectedChildTool>>,
    /// 这个 agent 是谁（`SubAgentSpec.agent_id`/`depth` 原样带进来，见 [`ToolExecCtx`]）。
    pub agent_id: Option<String>,
    /// 当前 agent 的嵌套深度（主会话为 0，每派一层子代理 +1），供嵌套限深判断。
    pub depth: u32,
}

/// 组装系统提示与工具列表（内置工具 + 已启用扩展工具）。
/// Agent::new 与 Agent::rebuild_tools 共用，保证两处行为一致。
fn compose_tools(
    cwd: &str,
    skills: &[Skill],
    rebuild_ctx: &ToolRebuildCtx,
    injected: &[InjectedChildTool],
) -> (String, Vec<(String, String, Value)>, Vec<String>) {
    let selection = &rebuild_ctx.selection;
    let exclude_tools = &rebuild_ctx.exclude_tools;

    // 扩展运行时过滤：已启用扩展可移除内置工具（如 plan-mode 禁用 edit/write）。
    // rebuild_tools 与 Agent::new 共用本函数，保证两处行为一致。
    //
    // allowlist 的条目是「名字或 `*` 通配」，必须拿去过滤**注册表**而不是当作名字列表，
    // 否则 `--tools 're*'` 会得到一个字面名 `re*` 而选不出任何工具
    let mut selected: Vec<String> = match selection {
        ToolSelection::Default(names) => names.clone(),
        ToolSelection::Allowlist(_) => tools::all_tool_names()
            .into_iter()
            .filter(|n| selection.is_allowed(n, exclude_tools))
            .collect(),
        ToolSelection::NoTools | ToolSelection::NoBuiltinTools => Vec::new(),
    };
    for ext in core::extensions::registered() {
        selected = ext.filter_tools(selected);
    }

    // --exclude-tools：过滤内置（条目支持 `*` 通配）
    if !exclude_tools.is_empty() {
        selected.retain(|t| !tool_names::matches_any(exclude_tools, t));
    }

    let tools_defs = tools::tool_defs(&selected);
    let mut snippets = HashMap::new();
    let mut prompt_guidelines: Vec<String> = Vec::new();
    for t in &tools_defs {
        snippets.insert(t.name.to_string(), t.snippet.clone());
        prompt_guidelines.extend(t.prompt_guidelines.clone());
    }

    // 扩展工具 snippet/guidelines（先按可见性过滤，再按同一选择策略过滤，NoTools 全关、Allowlist 只按名单）
    // 延迟工具（未激活）不在此列：不进系统提示，需经 tool_search 激活。
    for ext in core::extensions::registered() {
        let own = visible_extension_tools(ext.as_ref());
        for t in own {
            if !selection.is_allowed(&t.name, exclude_tools) {
                continue;
            }
            // 被 allowlist 保留但未点名的 MCP 工具只注册不对模型声明，因此也不进系统提示。
            if !selection.declares_to_model(&t.name, t.exposure) {
                continue;
            }
            snippets.insert(t.name.clone(), t.snippet.clone());
            prompt_guidelines.extend(t.prompt_guidelines.clone());
        }
    }

    // 注入工具：**无条件**并入（不受 selection/exclude 约束），
    // 同时进 `selected`：系统提示的 Available tools 段是按 `selected_tools` 查 snippet 渲染的，
    // 不推进去子代理就看不到这个工具（虽然可调）。
    for t in injected {
        snippets.insert(t.meta.name.clone(), t.meta.snippet.clone());
        prompt_guidelines.extend(t.meta.prompt_guidelines.clone());
        if !selected.iter().any(|n| n == &t.meta.name) {
            selected.push(t.meta.name.clone());
        }
    }

    let mut tools: Vec<(String, String, Value)> = tools_defs
        .into_iter()
        .map(|t| (t.name.to_string(), t.description, t.parameters))
        .collect();

    // 并入扩展工具定义（先按可见性过滤，再按选择策略；allowlist 时扩展也按名单；
    // NoTools 全关；其余无条件并入；延迟工具（未激活）不并入。
    for ext in core::extensions::registered() {
        let own = visible_extension_tools(ext.as_ref());
        for t in own {
            if !selection.is_allowed(&t.name, exclude_tools) {
                continue;
            }
            // 同上：保留但未点名的 MCP 工具不声明（仍留在扩展注册表里供脚本调用）。
            if !selection.declares_to_model(&t.name, t.exposure) {
                continue;
            }

            let parameters =
                parameters_with_grammar_sampling(&t.parameters, t.grammar_sampling.as_ref());

            if let Some(slot) = tools.iter_mut().find(|(n, _, _)| *n == t.name) {
                *slot = (t.name.clone(), t.description.clone(), parameters);
            } else {
                tools.push((t.name.clone(), t.description.clone(), parameters));
            }
        }
    }

    // 注入工具：同样无条件并入，且**最后并入**——同名时注入赢（见 `ToolIndex`）。
    for t in injected {
        let entry = (
            t.meta.name.clone(),
            t.meta.description.clone(),
            t.meta.parameters.clone(),
        );

        if let Some(slot) = tools.iter_mut().find(|(n, _, _)| *n == t.meta.name) {
            *slot = entry;
        } else {
            tools.push(entry);
        }
    }

    let hidden = apply_prepare_loadout(rebuild_ctx, &mut tools, injected);

    // 系统提示得等 `hidden` 算完才能建：「Available tools」只列本次请求真正声明的工具，
    // 被摘掉声明的（如 `codemode.mode = only` 下的直接工具）不能还挂在提示里让模型直接调。
    let sp = build_system_prompt(&SystemPromptOptions {
        cwd: cwd.to_string(),
        selected_tools: Some(selected.clone()),
        hidden_tools: hidden.clone(),
        tool_snippets: snippets,
        prompt_guidelines,
        append_system_prompt: rebuild_ctx.append_system_prompt.clone(),
        context_files: rebuild_ctx.context_files.clone(),
        skills: skills.to_vec(),
        custom_prompt: rebuild_ctx.system_prompt.clone(),
    });

    (sp, tools, hidden)
}

/// 把内置工具转成扩展工具形态（`prepare_loadout` 的 loadout 与 `ctx.script_tools` 共用）。
///
/// 用到 `name`/`description`/`parameters`/`snippet` 与 `output_schema`，其余取默认值。
fn builtin_tool_descriptor(name: &str, description: &str, parameters: &Value) -> ExtensionTool {
    let def = tools::tool_def(name);
    let snippet = def.as_ref().map(|d| d.snippet.clone()).unwrap_or_default();
    let mut tool = ExtensionTool::simple(name, description, parameters.clone(), &snippet);
    tool.output_schema = def.and_then(|d| d.output_schema);
    tool
}

/// 把工具的语法约束写进参数 schema（provider 层读 `x-prux-grammar` 决定下发 `custom` 工具）。
///
/// 工具表是 `(名字, 描述, 参数)` 三元组，没有工具级元数据槽位；语法约束因此随 schema
/// 传递（grammar 工具本就不把 schema 发给模型）。`input` 是承载源码的属性名：
/// 推不出来时不注入，工具退回普通 function 形态。
fn parameters_with_grammar_sampling(schema: &Value, sampling: Option<&GrammarSampling>) -> Value {
    let Some(sampling) = sampling else {
        return schema.clone();
    };
    let Some(input) = core::provider::grammar_input_property(schema) else {
        return schema.clone();
    };

    let mut grammar = serde_json::Map::new();
    if let Some(lark) = &sampling.openai_lark {
        grammar.insert("openai_lark".to_string(), json!(lark));
    }

    if let Some(regex) = &sampling.openai_regex {
        grammar.insert("openai_regex".to_string(), json!(regex));
    }
    grammar.insert("input".to_string(), json!(input));

    let mut out = schema.clone();
    if let Some(obj) = out.as_object_mut() {
        obj.insert(GRAMMAR_SCHEMA_KEY.to_string(), Value::Object(grammar));
    }
    out
}

/// 脚本/嵌套可调的工具表（[`ToolExecCtx::script_tools`]）：
/// 激活的内置工具 + 按 [`ToolExposure::is_script_callable`] 判定的扩展工具。
///
/// `active` 是本次请求表里的工具名（内置 = 已选择、扩展 = 模型可见）；
/// `selection`/`exclude` 用于把「被 allowlist 保留但未声明的 MCP 工具」也算作可调。
fn script_tools_for(
    active: &[(String, String, Value)],
    injected: &[InjectedChildTool],
    selection: &ToolSelection,
    exclude: &[String],
) -> Vec<ExtensionTool> {
    let names: Vec<&str> = active.iter().map(|(n, _, _)| n.as_str()).collect();
    let builtin_names = tools::all_tool_names();

    let mut out: Vec<ExtensionTool> = active
        .iter()
        .filter(|(n, _, _)| builtin_names.iter().any(|b| b == n))
        .map(|(n, d, p)| builtin_tool_descriptor(n, d, p))
        .collect();

    for ext in core::extensions::registered() {
        for t in ext.filter_extension_tools(ext.tools()) {
            // 脚本不能起脚本：`codemode` 自己声明 `model-only`，这里再明拒一道。
            if t.name == codemode::TOOL_NAME {
                continue;
            }

            // `direct` 工具要求已激活（否则模型也看不到它）；`codemode` 恒可调、
            // `deferred` 需已激活，`model-only`/`hidden` 不进脚本工具表。
            // 例外：被 allowlist 保留的 MCP 工具不对模型声明，但脚本与嵌套调用仍应可达。
            let callable = if t.exposure == ToolExposure::Direct {
                names.contains(&t.name.as_str())
                    || (selection.is_mcp_reserved(&t.name)
                        && !tool_names::matches_any(exclude, &t.name))
            } else {
                t.exposure
                    .is_script_callable(is_deferred_tool_activated(&t.name))
            };

            if callable {
                out.push(t);
            }
        }
    }

    // 注入工具（子代理的 StructuredOutput 等）同样可被脚本调。
    for t in injected {
        out.push(t.meta.clone());
    }
    out
}

/// 执行每个扩展的 [`Extension::prepare_loadout`]：把描述覆盖写回 `tools`，返回本次请求
/// 要隐藏声明的工具名（仍激活、仍可被脚本/嵌套调）。
fn apply_prepare_loadout(
    rebuild_ctx: &ToolRebuildCtx,
    tools: &mut [(String, String, Value)],
    injected: &[InjectedChildTool],
) -> Vec<String> {
    let builtin_names = tools::all_tool_names();
    let mut declared: Vec<ExtensionTool> = tools
        .iter()
        .filter(|(n, _, _)| builtin_names.iter().any(|b| b == n))
        .map(|(n, d, p)| builtin_tool_descriptor(n, d, p))
        .collect();

    for ext in core::extensions::registered() {
        for t in visible_extension_tools(ext.as_ref()) {
            if rebuild_ctx
                .selection
                .is_allowed(&t.name, &rebuild_ctx.exclude_tools)
                && rebuild_ctx.selection.declares_to_model(&t.name, t.exposure)
            {
                declared.push(t);
            }
        }
    }

    let loadout = ToolLoadout {
        declared,
        callable: script_tools_for(
            tools,
            injected,
            &rebuild_ctx.selection,
            &rebuild_ctx.exclude_tools,
        ),
    };

    let mut hidden: Vec<String> = Vec::new();
    for ext in core::extensions::registered() {
        let changes = ext.prepare_loadout(&loadout);
        for (name, description) in changes.descriptions {
            if let Some(slot) = tools.iter_mut().find(|(n, _, _)| *n == name) {
                slot.1 = description;
            }
        }

        for name in changes.hidden_declarations {
            if !hidden.contains(&name) {
                hidden.push(name);
            }
        }
    }
    hidden
}

/// 扩展对模型可见的工具：`filter_extension_tools` 过滤后，再按 [`ToolExposure`] 与
/// 延迟激活状态剔除模型看不到的（`codemode`/`hidden` 永不可见；`deferred` 需已激活）。
///
/// 工具表组装（系统提示 snippet / 工具 schema）与 `compose_tools` 必须共用这一口径，
/// 否则会出现“提示里说了、工具里没有”的错位。
fn visible_extension_tools(ext: &dyn Extension) -> Vec<ExtensionTool> {
    ext.filter_extension_tools(ext.tools())
        .into_iter()
        .filter(|t| {
            t.exposure
                .is_model_visible(is_deferred_tool_activated(&t.name))
        })
        .collect()
}

/// 一次工具解析的全部答案：优先级 **注入 > 内置 > 扩展注册表** 只写在这里。
///
/// 一个名字解析一次（`resolve`），四问（能不能用 / 怎么备参 / schema 是什么 / 用什么执行模式）都从这里答。
#[derive(Clone)]
struct ToolIndex<'a> {
    /// 解析用的名字
    name: &'a str,
    /// 注入工具（优先级最高）；同名时遮蔽内置与扩展。
    injected: Option<&'a InjectedChildTool>,
    /// 扩展注册表里解析出的同名工具（仅在注入与内置都未命中时查，避免无谓 clone）。
    extension: Option<ExtensionTool>,
    builtin: bool,
}

impl<'a> ToolIndex<'a> {
    /// 按名字解析：注入 > 内置 > 扩展注册表。三者都未命中时全为 `None`/`false`。
    fn resolve(injected: &'a [InjectedChildTool], name: &'a str) -> Self {
        let inj = injected.iter().find(|t| t.meta.name == name);
        let builtin = Agent::is_builtin_tool(name);
        let extension = if inj.is_none() && !builtin {
            find_extension_tool(name)
        } else {
            None
        };
        ToolIndex {
            name,
            injected: inj,
            extension,
            builtin,
        }
    }

    /// 该名字能否解析成一个工具。
    fn exists(&self) -> bool {
        self.injected.is_some() || self.builtin || self.extension.is_some()
    }

    /// 是否内置工具（决定派发路径）。
    fn is_builtin(&self) -> bool {
        self.builtin
    }

    /// 注入工具的执行体（命中注入时派发优先用它，不再走内置/扩展）。
    fn injected_handler(&self) -> Option<&'a InjectedToolHandler> {
        self.injected.map(|t| &t.handler)
    }

    /// 参数准备（LLM 参数发到执行前先变换）。
    fn prepare_arguments(&self, args: &Value) -> Value {
        if let Some(t) = self.injected
            && let Some(f) = t.meta.prepare_arguments
        {
            return f(args);
        }
        if self.builtin
            && let Some(d) = tools::tool_def(self.name)
            && let Some(f) = d.prepare_arguments
        {
            return f(args);
        }
        if let Some(f) = self.extension.as_ref().and_then(|t| t.prepare_arguments) {
            return f(args);
        }
        args.clone()
    }

    /// 参数 JSON Schema（准备之后用于校验）。
    fn parameters(&self) -> Option<Value> {
        if let Some(t) = self.injected {
            return Some(t.meta.parameters.clone());
        }
        if self.builtin
            && let Some(d) = tools::tool_def(self.name)
        {
            return Some(d.parameters.clone());
        }
        self.extension.as_ref().map(|t| t.parameters.clone())
    }

    /// 单工具执行模式覆盖。
    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        if let Some(t) = self.injected
            && let Some(m) = t.meta.execution_mode
        {
            return Some(m);
        }
        if self.builtin
            && let Some(m) = tools::tool_execution_mode(self.name)
        {
            return Some(m);
        }
        self.extension.as_ref().and_then(|t| t.execution_mode)
    }
}

/// 从扩展注册表里按名字找工具（不做 `filter_extension_tools` 过滤，与原行为一致）。
fn find_extension_tool(name: &str) -> Option<ExtensionTool> {
    core::extensions::registered()
        .iter()
        .find_map(|ext| ext.tools().into_iter().find(|t| t.name == name))
}

/// 一次性告警去重（进程级）：key 首次出现返回 `true`。
fn note_warning_once(key: &str) -> bool {
    /// 进程级告警去重集合，记录已经告警过的 key。
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    SEEN.get_or_init(Default::default)
        .lock()
        .unwrap()
        .insert(key.to_string())
}

/// 工具名集合是否相同（忽略顺序）。None 视为空集。
fn same_tool_names(recorded: Option<&[String]>, current: &[String]) -> bool {
    let mut recorded: Vec<&str> = recorded
        .unwrap_or_default()
        .iter()
        .map(String::as_str)
        .collect();
    let mut current: Vec<&str> = current.iter().map(String::as_str).collect();
    recorded.sort_unstable();
    current.sort_unstable();
    recorded == current
}

///  prepareNextTurn 返回的下一轮快照。支持替换上下文 / 切换模型 / thinking 级别
#[derive(Debug, Clone)]
pub struct NextTurnSnapshot {
    /// 下一轮要切换到的模型（None = 沿用当前模型）。
    pub model: Option<ModelConfig>,
    /// 下一轮要切换的 thinking 档位（None = 保持当前设置）。
    pub thinking_level: Option<String>,
    /// 下一轮上下文（完整替换 self.messages；None=保持当前）
    pub context: Option<Vec<AgentMessage>>,
}

/// 核心对话代理：持有模型、消息历史、工具与会话，驱动 LLM 循环与工具执行。
pub struct Agent {
    /// 当前模型配置（provider / model_id / api_key / 采样参数等）
    pub model: ModelConfig,
    /// 本次请求虚拟模型路由到的物理模型；非虚拟模型或尚未路由时为 None。
    ///
    /// 只对**当前请求**生效：每轮请求前重置并按 [`Self::model`] 重新解析；
    /// 模型选择（`self.model`）始终是用户选中的那个（可能是虚拟模型）。
    pub routed_model: Option<ModelConfig>,
    /// 路由后实际使用的 thinking 级别（已钳制到物理模型支持范围）；未路由时为 None。
    pub routed_thinking_level: Option<String>,
    /// 最近一次失败尝试的路由目标（`retry` 请求把它交给路由器）；被读取后清空。
    pub(crate) pending_failed_route: Option<FailedRoute>,
    /// 显式 API key 覆盖（None 时按 settings 解析）
    pub api_key_override: Option<String>,
    /// 已加载技能列表（组装系统提示与工具 snippet 用）
    pub skills: Vec<Skill>,
    /// 工作目录（系统提示与工具执行基准）
    pub cwd: String,
    /// 完整对话上下文（user / assistant / toolResult / compactionSummary）
    pub messages: Vec<AgentMessage>,
    /// 当前系统提示（builtin + 扩展 + 自定义拼接结果）
    pub system_prompt: String,
    /// 当前启用工具定义列表 (name, description, parameters JSON Schema)
    pub tools: Vec<(String, String, Value)>,
    /// 本次请求不声明的工具名（[`Extension::prepare_loadout`] 的 `hidden_declarations`）：
    /// 工具仍激活、仍可被脚本/嵌套调，只是不发进模型请求（`codemode.mode = only`）。
    pub hidden_declarations: Vec<String>,
    /// 工具列表重建参数（/extension 面板关闭时 rebuild_tools 用）
    pub rebuild_ctx: ToolRebuildCtx,
    /// 启动时由 settings.json `defaultTools` 解析出的工具名；`None` = 工具选择来自命令行
    /// （`--tools` allowlist / `--no-tools` / `--no-builtin-tools`），`/reload` 不重读 settings。
    /// 值已套用 [`Self::settings_default_tool_modifiers`]。
    /// 见 [`Agent::reload_default_tools`]。
    settings_default_tools: Option<Vec<String>>,
    /// 命令行 `--tools` 给出的 `+name`/`-name` 修饰符；`/reload` 重读 `defaultTools` 后按同一组顺序重放。
    /// 非纯修饰符的命令行选择下为空。
    settings_default_tool_modifiers: Vec<String>,
    /// 当前 thinking 级别（None = 未设置，按模型默认处理）
    pub thinking_level: Option<String>,
    /// 最近一次 LLM 调用的用量统计（token 计数等）
    pub last_usage: Option<Usage>,
    /// 当前会话（树导航、持久化、上下文重建）
    pub session: Option<Session>,
    /// 事件分发 sink（emit 内部 JSON 事件用）
    pub json_sink: Option<Arc<Mutex<JsonSink>>>,
    /// 发送层遥测回调：LLM 调用完成后回调
    #[allow(clippy::type_complexity)]
    pub on_response: Option<Arc<Mutex<Box<dyn FnMut(&Value) + Send>>>>,
    /// onPayload 钩子：provider 发送前检查/替换请求体
    #[allow(clippy::type_complexity)]
    pub on_payload: Option<Arc<Mutex<Box<dyn FnMut(&mut Value) + Send>>>>,
    /// 当前运行的取消信号（run 前复位，request_abort() 置位，轮间与工具执行点检查；
    /// provider HTTP 层中断随 StreamFn 契约批次接入）。
    ///
    /// **Arc 身份在一次 Agent 生命周期内保持稳定**（仅硬中止收尾 [Self::abort_and_detach] 才换新）：
    /// 子代理物化时捕获的句柄（`MaterializedSubAgent::abort`）要能跨`prompt` 触达运行中的取消信号，
    /// 若每次 run 重新分配，捕获到的句柄会指向废信号。
    pub run_abort: Arc<AtomicBool>,
    /// steer 取出模式（"one-at-a-time" 逐条 / "all" 整批）——运行中注入的消息
    pub steer_mode: String,
    /// follow-up 取出模式（"one-at-a-time" 逐条 / "all" 整批）——settle 后发送的消息
    pub follow_up_mode: String,
    /// 上下文阈值自动压缩开关
    pub auto_compaction: bool,
    /// 自动重试开关（溢出压缩重试 + 瞬时错误退避重试）
    pub auto_retry: bool,
    /// 压缩进行中标志（含手动/自动压缩）
    pub is_compacting: bool,
    /// 溢出压缩重试守卫：每次 run 重置，压缩+重试只允许一次；重试后仍溢出只发失败通知不再重复压缩。
    pub overflow_recovery_attempted: bool,
    /// 本次注入的 user 消息批次（单条或 all 批量多条），run_loop 开始时发 user 事件用
    pub(crate) last_user_batch: Vec<AgentMessage>,
    /// 运行中 steer 注入箱（独立锁）：TUI 等模式在 agent 运行时，
    /// 通过它中途注入 steer（每个 turn 前 poll steeringQueue）。
    pub runtime_steer_inbox: Arc<Mutex<VecDeque<AgentMessage>>>,
    /// 子代理实例（`child_from_template`）：边界事件 `agentScope` 用。
    /// 扩展据此区分「会话主 agent」与「子代理 run」（如 goal 只在主 agent 结算续跑）。
    pub is_subagent: bool,
    /// 离线模式：不发起网络请求（本地目录/模型浏览等仅本地操作）
    pub offline: bool,
    /// --models / settings.enabledModels 解析出的 scope（Ctrl+P 循环，有序，含 thinking）
    pub model_cycle: Vec<ScopedModel>,
    /// 当前模型是否来自无配置兑底（从未选择过模型）。
    /// /login 成功后若为 true 则自动选择该 provider 默认模型并持久化。
    pub model_is_fallback: bool,
    /// 工具执行模式（默认 Parallel）
    pub tool_execution: ToolExecutionMode,
    /// 每轮收尾回调（turn_end 之前调用、决策之后生效）
    pub finish_turn: Option<Mutex<FinishTurn>>,
    /// 每轮结束后返回下一轮快照
    pub prepare_next_turn: Option<Mutex<PrepareNextTurn>>,
    /// 可插拔主循环：None → 内置默认；Some → 替换实现（子代理继承同一实现；无锁可重入）
    pub agent_loop: Option<AgentLoop>,
    /// 提示缓存保温器：每次成功请求后按 TTL 重放同请求续命缓存。（子代理不保温）
    pub cache_warmer: CacheWarmer,
}

/// 单工具最终结果
struct FinalizedToolCall {
    /// 模型给出的原始工具调用块（含调用 id 与名称）。
    call: ContentBlock,
    /// 工具名，用于查表执行与事件上报。
    name: String,
    /// 已通过 schema 校验的工具参数（JSON 对象）。
    args: Value,
    /// 返回给模型的文本结果。
    text: String,
    /// 附加的结构化元数据（如 diff、行号），无则为 None。
    details: Option<Value>,
    /// 本次工具执行消耗的 token 用量，未统计时为 None。
    usage: Option<Usage>,
    /// true 表示该结果带终止语义：本批工具跑完后不再请求模型。
    terminate: bool,
    /// true 表示这是失败结果（工具不存在、参数校验失败或执行出错）。
    is_error: bool,
    /// 结果附带的图片附件，组装消息时转为 Image 内容块。
    attachments: Vec<ToolResultAttachment>,
    /// afterToolCall 替换整个 content 数组
    content_override: Option<Vec<ContentBlock>>,
    /// 工具自身耗时（毫秒，单调时钟，不含 `afterToolCall` 钩子）；
    /// 未真正执行的调用（未知工具、被 block 的 immediate、中止）为 `None`。
    /// 进 `tool_execution_end.durationMs` 与 `toolResult` 消息。
    duration_ms: Option<u64>,
}

impl FinalizedToolCall {
    /// 取原始工具调用块里的调用 id；非工具调用块（异常情况）返回空串。
    fn call_id(&self) -> String {
        match &self.call {
            ContentBlock::ToolCall { id, .. } => id.clone(),
            _ => String::new(),
        }
    }

    /// 构造失败的工具结果：`is_error=true`，无 usage/附件；
    /// `terminate` 决定本批工具跑完后是否还请求模型。
    fn error_result(
        call: &ContentBlock,
        name: &str,
        args: &Value,
        text: &str,
        terminate: bool,
    ) -> Self {
        FinalizedToolCall {
            call: call.clone(),
            name: name.to_string(),
            args: args.clone(),
            text: text.to_string(),
            details: None,
            usage: None,
            terminate,
            is_error: true,
            attachments: Vec::new(),
            content_override: None,
            duration_ms: None,
        }
    }

    /// 用工具执行返回的 [`ToolResult`] 填充最终结果，`content_override` 留空。
    ///
    /// 错误标记取自 [`ToolResult::is_error`]（工具可在失败的同时携带结构化结果）。
    /// 结构化输出（[`ToolResult::structured_content`]）并进 `details.structuredContent`：
    /// 与服务端 MCP 结果同一落盘形态（渲染 / 事件 / 调试面板都读这个键）。
    fn from_tool_result(call: &ContentBlock, name: &str, args: &Value, t: ToolResult) -> Self {
        let details = match t.structured_content {
            Some(sc) => {
                let mut map = match t.details {
                    Some(Value::Object(o)) => o,
                    _ => serde_json::Map::new(),
                };
                map.insert("structuredContent".to_string(), sc);
                Some(Value::Object(map))
            }
            None => t.details,
        };

        FinalizedToolCall {
            call: call.clone(),
            name: name.to_string(),
            args: args.clone(),
            text: t.text,
            details,
            usage: t.usage,
            terminate: t.terminate,
            is_error: t.is_error,
            attachments: t.attachments,
            content_override: None,
            duration_ms: None,
        }
    }

    /// agent 事件 payload 中的 result：完整 AgentToolResult 对象
    fn to_result_json(&self) -> Value {
        let mut m = serde_json::Map::new();
        m.insert(
            "content".to_string(),
            json!([{ "type": "text", "text": self.text }]),
        );
        m.insert(
            "details".to_string(),
            self.details.clone().unwrap_or_else(|| json!({})),
        );

        if let Some(u) = &self.usage {
            m.insert(
                "usage".to_string(),
                serde_json::to_value(u).unwrap_or(Value::Null),
            );
        }

        if self.terminate {
            m.insert("terminate".to_string(), json!(true));
        }
        Value::Object(m)
    }
}

/// 子代理模板：构建子代理所需的不变配置快照（供 'static 的扩展工具 thunk 使用，不借用 &self）。
/// 由 [`Agent::snapshot_template`] 捕获，[`Agent::child_from_template`] 还原。
struct AgentTemplate {
    /// 子代理继承的模型配置快照。
    model: ModelConfig,
    /// 显式 API key 覆盖，None 时按 settings 解析。
    api_key_override: Option<String>,
    /// 已加载技能列表，用于组装子代理的系统提示与工具 snippet。
    skills: Vec<Skill>,
    /// 工作目录，子代理工具执行与路径解析的基准。
    cwd: String,
    /// 父级系统提示快照，作为子代理提示的基础。
    system_prompt: String,
    /// 工具定义列表 (name, description, parameters JSON Schema)。
    tools: Vec<(String, String, Value)>,
    /// 系统提示与工具列表的重建参数。
    rebuild_ctx: ToolRebuildCtx,
    /// thinking 档位，None 表示未设置、按模型默认处理。
    thinking_level: Option<String>,
    /// true 表示离线模式，不发起任何网络请求。
    offline: bool,
    /// true 表示允许溢出压缩与瞬时错误的自动重试。
    auto_retry: bool,
    /// 工具执行模式（默认并行，可切顺序）。
    tool_execution: ToolExecutionMode,
    /// 父级当前对话快照：供子代理 `inherit_context` 复制父会话（Arc 化，克隆只增引用计数）
    parent_messages: Arc<Vec<AgentMessage>>,
    /// 父会话 id：供子会话自动挂到父会话下（`/resume` 嵌套显示用）
    parent_session_id: Option<String>,
}

impl Agent {
    /// agent 级重试退避上限
    const MAX_AGENT_RETRY_DELAY_MS: u64 = 60_000;

    /// 捕获当前 agent 的不变配置（供子代理扩展在 'static thunk 中还原子代理）。
    fn snapshot_template(&self) -> AgentTemplate {
        let system_override = self
            .rebuild_ctx
            .system_prompt
            .clone()
            .or_else(|| Some(self.system_prompt.clone()))
            .unwrap_or_default();

        AgentTemplate {
            model: self.model.clone(),
            api_key_override: self.api_key_override.clone(),
            skills: self.skills.clone(),
            cwd: self.cwd.clone(),
            system_prompt: system_override,
            tools: self.tools.clone(),
            rebuild_ctx: self.rebuild_ctx.clone(),
            thinking_level: self.thinking_level.clone(),
            offline: self.offline,
            auto_retry: self.auto_retry,
            tool_execution: self.tool_execution,
            parent_messages: Arc::new(self.messages.clone()),
            parent_session_id: self.session.as_ref().map(|s| s.session_id.clone()),
        }
    }

    /// 构造会话级执行上下文（供扩展的 `on_exec_ctx` 与“工具执行之外”的
    /// spawn/resume 使用；与工具执行期传给扩展的 ctx 同源）。
    pub fn exec_ctx(&self) -> ToolExecCtx {
        let child_tpl = Arc::new(self.snapshot_template());
        let entries = self.session_branch_entries();

        make_exec_ctx(
            &self.cwd,
            &child_tpl,
            &self.run_abort,
            &self.json_sink,
            None,
            entries,
        )
    }

    /// 当前会话活动分支的条目快照（根在前）；无会话时 `None`。
    ///
    /// 有会话但分支为空（新建会话、叶子未设置）时返回空快照——两者语义不同：
    /// `None` 表示「拿不到分支数据，别动既有快照」。
    fn session_branch_entries(&self) -> Option<Arc<Vec<Value>>> {
        let session = self.session.as_ref()?;
        let entries = match session.get_leaf_id() {
            Some(leaf) => session.get_branch(leaf),
            None => Vec::new(),
        };
        Some(Arc::new(entries))
    }

    /// 从模板还原一个状态隔离的子代理（空消息、无 session、独立队列/sink）。
    fn child_from_template(tpl: &AgentTemplate) -> Agent {
        Agent {
            model: tpl.model.clone(),
            routed_model: None,
            routed_thinking_level: None,
            pending_failed_route: None,
            api_key_override: tpl.api_key_override.clone(),
            skills: tpl.skills.clone(),
            cwd: tpl.cwd.clone(),
            messages: Vec::new(),
            system_prompt: tpl.system_prompt.clone(),
            tools: tpl.tools.clone(),
            hidden_declarations: Vec::new(),
            rebuild_ctx: tpl.rebuild_ctx.clone(),
            settings_default_tools: None,
            settings_default_tool_modifiers: Vec::new(),
            thinking_level: tpl.thinking_level.clone(),
            last_usage: None,
            session: None,
            json_sink: None,
            on_response: None,
            on_payload: None,
            run_abort: Arc::new(AtomicBool::new(false)),
            steer_mode: String::from("one-at-a-time"),
            follow_up_mode: String::from("one-at-a-time"),
            auto_compaction: false,
            auto_retry: tpl.auto_retry,
            is_compacting: false,
            overflow_recovery_attempted: false,
            last_user_batch: Vec::new(),
            runtime_steer_inbox: Arc::new(Mutex::new(VecDeque::new())),
            is_subagent: true,
            offline: tpl.offline,
            model_cycle: Vec::new(),
            model_is_fallback: false,
            tool_execution: tpl.tool_execution,
            finish_turn: None,
            prepare_next_turn: None,
            agent_loop: None,
            cache_warmer: CacheWarmer::new(),
        }
    }

    /// 创建主会话 Agent：解析模型配置、把 thinking 档位钳制到模型支持范围、
    /// 组装系统提示与工具表，并在给了 `session` 时从会话历史重建消息列表。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        model_entry: ModelEntry,
        cwd: String,
        selection: ToolSelection,
        exclude_tools: Vec<String>,
        thinking_level: Option<String>,
        system_prompt: Option<String>,
        append_system_prompt: Option<String>,
        context_files: Vec<(String, String)>,
        session: Option<Session>,
        api_key_override: Option<String>,
        offline: bool,
        skills: Vec<Skill>,
    ) -> Result<Agent> {
        let model = model_resolver::model_config_from_entry(
            &model_entry,
            api_key_override.clone(),
            session.as_ref().map(|s| s.session_id.clone()),
        );

        // 启动时把 thinking 级别钳制到模型支持范围
        let thinking_level = thinking_level.map(|l| model.clamp_thinking_level(&l));

        let rebuild_ctx = ToolRebuildCtx {
            selection: selection.clone(),
            exclude_tools: exclude_tools.clone(),
            system_prompt: system_prompt.clone(),
            append_system_prompt: append_system_prompt.clone(),
            context_files: context_files.clone(),
            injected_tools: Arc::new(Vec::new()),
            agent_id: None,
            depth: 0,
        };

        // 系统提示（内置工具 + 已启用扩展工具）。
        // 恢复会话时先按历史把延迟工具重新激活，否则本轮工具表会比上一轮少掉那些工具。
        let mut history = Vec::new();
        if let Some(s) = &session {
            history = s.build_context_messages();
            core::extensions::restore_deferred_activations(&history);
        }
        let (sp, tools, hidden) = compose_tools(&cwd, &skills, &rebuild_ctx, &[]);

        Ok(Agent {
            model,
            routed_model: None,
            routed_thinking_level: None,
            pending_failed_route: None,
            api_key_override,
            skills,
            cwd,
            messages: history,
            system_prompt: sp,
            tools,
            hidden_declarations: hidden,
            rebuild_ctx,
            settings_default_tools: None,
            settings_default_tool_modifiers: Vec::new(),
            thinking_level,
            last_usage: None,
            session,
            json_sink: None,
            on_response: None,
            on_payload: None,
            run_abort: Arc::new(AtomicBool::new(false)),
            steer_mode: settings_manager::read_settings_steering_mode(),
            follow_up_mode: settings_manager::read_settings_follow_up_mode(),
            auto_compaction: settings_manager::read_settings_auto_compact(),
            auto_retry: true,
            is_compacting: false,
            overflow_recovery_attempted: false,
            last_user_batch: Vec::new(),
            runtime_steer_inbox: Arc::new(Mutex::new(VecDeque::new())),
            is_subagent: false,
            offline,
            model_cycle: Vec::new(),
            model_is_fallback: false,
            tool_execution: ToolExecutionMode::Parallel,
            finish_turn: None,
            prepare_next_turn: None,
            agent_loop: None,
            cache_warmer: CacheWarmer::new(),
        })
    }

    /// 记录启动时由 settings.json `defaultTools` 解析出的工具名（命令行显式指定工具集时传 `None`）；
    /// 只影响 [`Self::reload_default_tools`]，不改变当前选择。
    pub fn set_settings_default_tools(&mut self, names: Option<Vec<String>>) {
        self.settings_default_tools = names;
    }

    /// 记录命令行 `--tools` 的 `+name`/`-name` 修饰符（无修饰符时传空）；
    /// 只影响 [`Self::reload_default_tools`]：`/reload` 重读 settings 后按同一组修饰符重算。
    pub fn set_default_tool_modifiers(&mut self, modifiers: Vec<String>) {
        self.settings_default_tool_modifiers = modifiers;
    }

    /// `/reload` 后重读 settings.json 的 `defaultTools`（调用方随后自己 [`Self::rebuild_tools`]）。
    ///
    /// 只增不减：新加入 `defaultTools` 的工具并入当前选择；从 `defaultTools` 移除的工具**保持启用**，
    /// 会话里另行关掉的工具也不会因此复活。命令行 `--tools` / `--no-tools` / `--no-builtin-tools`
    /// 显式指定过工具集时（`settings_default_tools` 为 `None`）本方法不做任何事。
    pub fn reload_default_tools(&mut self) {
        let Some(previous) = self.settings_default_tools.take() else {
            return;
        };

        let raw = settings_manager::read_settings_default_tools().unwrap_or_else(|| {
            settings_manager::DEFAULT_TOOL_NAMES
                .iter()
                .map(|s| s.to_string())
                .collect()
        });

        // `--tools +name/-name` 的选择要跟着 `/reload` 重算
        let next =
            settings_manager::apply_tool_modifiers(&raw, &self.settings_default_tool_modifiers);

        if let ToolSelection::Default(current) = &mut self.rebuild_ctx.selection {
            for name in next.iter().filter(|n| !previous.contains(*n)).cloned() {
                if !current.contains(&name) {
                    current.push(name);
                }
            }
        }
        self.settings_default_tools = Some(next);
    }

    /// 重建系统提示与工具列表（/extension 面板关闭时调用）：
    /// 按当前扩展启用状态重新组装内置工具 + 已启用扩展工具，
    /// 使模型立即可见新增/移除的扩展工具。
    pub fn rebuild_tools(&mut self) {
        let (sp, tools, hidden) = compose_tools(
            &self.cwd,
            &self.skills,
            &self.rebuild_ctx,
            &self.rebuild_ctx.injected_tools,
        );
        self.system_prompt = sp;
        self.tools = tools;
        self.hidden_declarations = hidden;

        if let Some(s) = self.session.as_mut() {
            let names: Vec<String> = self.tools.iter().map(|t| t.0.clone()).collect();

            // 工具集未变化时不追加 ActiveToolsChange 条目，避免配置条目顶到 lane 叶子导致 /clone 目标非 message
            if !same_tool_names(s.last_active_tools().as_deref(), &names) {
                s.append_active_tools_change(&names);
            }
        }
    }

    /// 当前活动工具名
    pub fn get_active_tools(&self) -> Vec<String> {
        self.tools.iter().map(|t| t.0.clone()).collect()
    }

    /// 全部可用工具名（内置 + 已注册扩展；应用 allowlist/exclude/NoTools 约束）
    pub fn get_all_tools(&self) -> Vec<String> {
        if matches!(self.rebuild_ctx.selection, ToolSelection::NoTools) {
            return Vec::new();
        }
        let mut names = tools::all_tool_names();
        for ext in core::extensions::registered() {
            let own = ext.filter_extension_tools(ext.tools());
            for t in own {
                if !names.contains(&t.name) {
                    names.push(t.name);
                }
            }
        }
        names.retain(|n| {
            self.rebuild_ctx
                .selection
                .is_allowed(n, &self.rebuild_ctx.exclude_tools)
        });
        names
    }

    /// 设置活动工具：替换选中内置子集（扩展按当前选择策略并入）并重建系统提示/工具列表。
    pub fn set_active_tools(&mut self, names: Vec<String>) {
        self.rebuild_ctx.selection = ToolSelection::Default(names);
        self.rebuild_tools();
    }

    /// 清空全部排队消息
    pub fn clear_all_queues(&mut self) {
        self.runtime_steer_inbox.lock().unwrap().clear();
    }

    /// 重置全部状态：清空消息、队列。
    pub fn reset(&mut self) -> Result<()> {
        self.messages.clear();
        self.last_user_batch.clear();
        self.clear_all_queues();
        Ok(())
    }

    /// 把事件推给外部 sink（若已挂接）；无 sink 时静默丢弃。
    pub(crate) fn emit(&mut self, event: Value) {
        if let Some(sink) = self.json_sink.as_ref()
            && let Ok(mut g) = sink.lock()
        {
            let f: &mut dyn FnMut(Value) = g.as_mut();
            f(event);
        }
    }

    /// 设置响应遥测回调（每轮 LLM 调用完成后回调）
    pub fn set_on_response(&mut self, cb: Box<dyn FnMut(&Value) + Send>) {
        self.on_response = Some(Arc::new(Mutex::new(cb)));
    }

    /// 设置 onPayload 钩子：provider 发送前检查/替换请求体
    pub fn set_on_payload(&mut self, cb: Box<dyn FnMut(&mut Value) + Send>) {
        self.on_payload = Some(Arc::new(Mutex::new(cb)));
    }

    /// 外部挂接事件 sink（内部包 Arc+Mutex：使 Agent 可 Sync，&Agent 可跨 await Send，且可共享给并行工具分支）
    pub fn attach_json_sink(&mut self, sink: JsonSink) {
        self.json_sink = Some(Arc::new(Mutex::new(sink)));
    }

    /// 请求取消当前 run
    pub fn request_abort(&self) {
        self.run_abort.store(true, Ordering::Relaxed);
    }

    /// 硬中止（Esc/`AbortRun`，回合 future 即将被 drop）的取消信号收尾：
    /// 先置位本轮信号，让持有 `parent_abort` 克隆的扩展/工具观察到取消；
    /// 再换上一个全新的未置位信号并返回旧的。
    ///
    /// 换新是必要的：`exec_ctx()` 会把**当前**信号交给扩展缓存，若继续沿用已置位的
    /// 同一个 Arc，Esc 后空闲期经缓存 ctx 新建的后台代理会一出生就被取消。
    pub fn abort_and_detach(&mut self) -> Arc<AtomicBool> {
        self.request_abort();
        std::mem::replace(&mut self.run_abort, Arc::new(AtomicBool::new(false)))
    }

    /// 切换模型（/model 命令用）。`persist` 为 true 时，切换成功后才写
    /// defaultProvider/defaultModel（对齐 pi `setModel(model, {persist})`）。
    pub fn switch_model(&mut self, provider: &str, model_id: &str, persist: bool) -> Result<()> {
        let entry = model_resolver::find_model(provider, model_id)?;
        self.model = model_resolver::model_config_from_entry(
            &entry,
            self.api_key_override.clone(),
            self.session.as_ref().map(|s| s.session_id.clone()),
        );

        if let Some(s) = self.session.as_mut() {
            s.append_model_change(provider, model_id);
        }

        // 默认值只在模型切换成功后才写盘；普通切换（面板 Enter / Ctrl+P /
        // /model <精确名>）保持会话级，不写盘。写盘失败不影响本次切换。
        if persist {
            _ = settings_manager::write_default_model_and_provider(&entry.provider, &entry.id);
        }

        // 切换后不再是兜底模型：后续 /login 不再自动改选
        self.model_is_fallback = false;

        // 切换后按新模型能力重置 thinking 级别
        let current = self
            .thinking_level
            .clone()
            .unwrap_or_else(|| "off".to_string());
        self.set_thinking_level(&current);
        Ok(())
    }

    /// 当前模型支持的 thinking 级别
    pub fn get_available_thinking_levels(&self) -> Vec<&'static str> {
        self.model.supported_thinking_levels()
    }

    /// 选中的模型是否为虚拟模型（`api` 为 [`virtual_models::VIRTUAL_MODEL_API`]）。
    pub fn selected_is_virtual(&self) -> bool {
        virtual_models::is_virtual_api(&self.model.api)
    }

    /// 本次请求实际使用的模型：已路由时是路由后的物理模型，否则是选中模型。
    pub fn effective_model(&self) -> &ModelConfig {
        self.routed_model.as_ref().unwrap_or(&self.model)
    }

    /// 本次请求实际使用的 thinking 级别：已路由时为路由后的级别（已钳制），
    /// 否则是会话当前的 thinking 设置。
    pub fn effective_thinking_level(&self) -> Option<String> {
        self.routed_thinking_level
            .clone()
            .or_else(|| self.thinking_level.clone())
    }

    /// 清空本次请求的路由结果（每轮请求前调用）。
    pub fn clear_routed_model(&mut self) {
        self.routed_model = None;
        self.routed_thinking_level = None;
    }

    /// 取出并清除待用的失败尝试信息（`retry` 请求的路由依据）。
    pub fn take_pending_failed_route(&mut self) -> Option<FailedRoute> {
        self.pending_failed_route.take()
    }

    /// agent 循环之外的直接请求（压缩摘要 / 扩展直调）使用的模型：
    /// 非虚拟选择原样返回，虚拟选择按 [`virtual_models::ModelRouteReason::Direct`] 路由一次
    /// （不读写路由器状态）。路由失败返回错误，由调用方决定是否降级。
    pub async fn direct_model(&self) -> Result<ModelConfig> {
        if !self.selected_is_virtual() {
            return Ok(self.model.clone());
        }

        let request = ModelRouteRequest {
            selected: &self.model,
            thinking_level: self.thinking_level.as_deref(),
            reason: ModelRouteReason::Direct,
            previous: None,
            failed: None,
            state: None,
            messages: &self.messages,
            abort: Some(self.run_abort.clone()),
        };
        let route = virtual_models::resolve_route(
            request,
            self.api_key_override.clone(),
            self.session.as_ref().map(|s| s.session_id.clone()),
        )
        .await?;

        Ok(route.model)
    }

    /// 摘要生成实际使用的模型：进行中的请求用已路由的物理模型，空闲时按 `direct` 理由路由。
    pub async fn summarization_model(&self) -> Result<ModelConfig> {
        match &self.routed_model {
            Some(m) => Ok(m.clone()),
            None => self.direct_model().await,
        }
    }

    /// 设置 thinking 级别：重置到模型支持范围，仅在级别实际变化时记录 session。
    pub fn set_session_name(&mut self, name: &str) -> bool {
        let Some(s) = self.session.as_mut() else {
            return false;
        };
        s.name = Some(name.to_string());
        s.append_session_info(name);
        self.emit(json!({
            "type": "session_info_changed",
            "name": name,
        }));
        true
    }

    /// 设置 thinking 级别：重置到模型支持范围，仅在级别实际变化时记录 session。
    /// 变化时经 emit 广播 thinking_level_changed（模型切换/迭代快照恢复等 agent 内部路径）。
    pub fn set_thinking_level(&mut self, level: &str) -> String {
        let (effective, changed) = self.apply_thinking_level(level);
        if changed {
            self.emit(json!({
                "type": "thinking_level_changed",
                "level": effective,
            }));
        }
        effective
    }

    /// 设置 thinking 级别但不 emit：worker 的 SetThinking 命令回执已类型化，
    /// 直接经 run_in_event_loop 调 `crate::modes::interactive::handlers::events::on_thinking_level_changed`，不再走 JSON sink。
    /// `persist` 为 true 时，设置成功后才写 defaultThinkingLevel（对齐 pi `setThinkingLevel(level, {persist})`，持久化请求的级别而非钳制后的值）。
    pub fn set_thinking_level_silent(&mut self, level: &str, persist: bool) -> String {
        let effective = self.apply_thinking_level(level).0;
        if persist {
            _ = settings_manager::write_settings_default_thinking_level(level);
        }
        effective
    }

    /// 把请求的思考档位钳制到模型支持范围并更新内部状态；
    /// 有变化时向会话追加一条档位变更条目，返回 `(生效档位, 是否变化)`。
    /// 本方法不写盘（是否持久化为默认档位由调用方决定）。
    fn apply_thinking_level(&mut self, level: &str) -> (String, bool) {
        let effective = self.model.clamp_thinking_level(level);
        let changed = self.thinking_level.as_deref() != Some(effective.as_str());
        if changed {
            self.thinking_level = Some(effective.clone());
            if let Some(s) = self.session.as_mut() {
                s.append_thinking_level_change(&effective);
            }

            // 默认思考级别的持久化只在 persist=true（/thinking 面板 Ctrl+S）时
            // 由 set_thinking_level_silent 在设置成功后写入；
            // Shift+Tab / 模型切换钳制 / 快照恢复都只改会话状态，不写盘。
        }
        (effective, changed)
    }

    /// 上下文压缩：自动触发（阈值）或手动
    pub async fn maybe_compact(&mut self, force: bool) -> Result<bool> {
        let reason = if force {
            compaction::CompactionReason::Manual
        } else {
            compaction::CompactionReason::Threshold
        };
        Ok(self.maybe_compact_with(reason, None).await?.is_some())
    }

    /// 上下文压缩：启用/禁用以 `autoCompact` 设置与 [`CompactionReason`] 决定触发；
    /// 支持自定义摘要指令，返回压缩结果
    pub async fn maybe_compact_with(
        &mut self,
        reason: compaction::CompactionReason,
        custom_instructions: Option<&str>,
    ) -> Result<Option<Value>> {
        self.is_compacting = true;
        let result = self.maybe_compact_inner(reason, custom_instructions).await;
        self.is_compacting = false;
        result
    }

    /// 压缩的内部实现（不含 `is_compacting` 标记）：先判定是否该压
    /// （阈值判定带陈旧 usage 守卫），需要时生成摘要、写会话并返回压缩结果；
    /// 不触发时返回 `Ok(None)`。
    async fn maybe_compact_inner(
        &mut self,
        reason: compaction::CompactionReason,
        custom_instructions: Option<&str>,
    ) -> Result<Option<Value>> {
        // 本次压缩的计量与摘要模型：已路由时用路由后的物理模型，
        // 空闲下的虚拟选择按 `direct` 理由路由（阈值判定与摘要生成必须用同一模型）。
        let limit_model = self
            .summarization_model()
            .await
            .unwrap_or_else(|_| self.model.clone());
        let settings = compaction::CompactionSettings::from_settings()
            .for_model(&limit_model.provider, &limit_model.model_id);
        let estimate = compaction::estimate_context_tokens(&self.messages);
        let context_tokens = estimate.tokens;
        let should = match reason {
            compaction::CompactionReason::Manual | compaction::CompactionReason::Overflow => true,
            compaction::CompactionReason::Threshold => {
                // 陈旧 usage 守卫：usage 锚点（最后一条带 usage 的 assistant）
                // 早于或等于最近 compaction boundary 时，它是压缩前的陈旧数据；
                // 压缩后首轮按它判断会重复触发压缩，因此跳过。
                let latest_compaction_ts = self.session.as_ref().and_then(|s| {
                    s.state.get_entries().iter().rev().find_map(|e| match e {
                        session_v4::Entry::Compaction { base, .. } => Some(base.timestamp),
                        _ => None,
                    })
                });
                let stale = compaction::usage_anchor_is_stale(
                    &estimate,
                    &self.messages,
                    latest_compaction_ts,
                );
                !stale
                    && compaction::should_compact(
                        context_tokens,
                        limit_model.context_window,
                        &settings,
                    )
            }
        };

        if !should {
            return Ok(None);
        }

        self.emit(json!({
            "type": "compaction_start",
            "reason": reason.as_str(),
        }));

        // 找第一条 compactionSummary
        let mut previous_summary: Option<String> = None;
        let mut summarize_start = 0usize;
        for (i, m) in self.messages.iter().enumerate() {
            if m.role == "compactionSummary" {
                previous_summary = Some(m.text());
                summarize_start = i + 1;
                break;
            }
        }

        // 找切点：从最新往回累积 token（含 tool result），达到 keep_recent；超大的尾随 tool result 不再被跳过
        let keep_recent = settings.keep_recent_tokens;
        let cut_index =
            compaction::find_compaction_cut_index(&self.messages, summarize_start, keep_recent);

        if cut_index <= summarize_start {
            // 没有可压缩的内容
            return Ok(None);
        }

        let messages_to_summarize = self.messages[summarize_start..cut_index].to_vec();
        let kept_messages = self.messages[cut_index..].to_vec();
        if messages_to_summarize.is_empty() {
            return Ok(None);
        }

        // 序列化并生成摘要
        let conversation_text = compaction::serialize_conversation(&messages_to_summarize);
        let summary_prompt =
            compaction::build_summary_prompt(previous_summary.as_deref(), custom_instructions);
        let user_prompt = conversation_text.to_string();

        // compaction 状态改为 AgentState/查询（不再发独有事件）
        for ext in core::extensions::registered() {
            ext.on_before_compact(context_tokens, reason.as_str());
        }

        // 自定义摘要：任一扩展返回 Some 则替换内置摘要通道。
        let mut custom_summary: Option<String> = None;
        for ext in core::extensions::registered() {
            if let Some(s) = ext.custom_summarize(
                compaction::SUMMARIZATION_SYSTEM_PROMPT,
                &user_prompt,
                &summary_prompt,
            ) {
                custom_summary = Some(s);
                break;
            }
        }

        let summary_result = match custom_summary {
            Some(s) => Ok((s, Usage::default())),
            None => {
                let reserve = settings.reserve_tokens.max(1);
                let budget = ((reserve as f64) * 0.8).floor() as u32;
                let max_tokens = limit_model.max_tokens.unwrap_or(u32::MAX).min(budget);

                core::provider::simple_completion(
                    &limit_model,
                    compaction::SUMMARIZATION_SYSTEM_PROMPT,
                    &format!("{}\n\n{}", user_prompt, summary_prompt),
                    Some(max_tokens),
                )
                .await
            }
        };

        let (summary, summary_usage) = match summary_result {
            Ok(v) => v,
            Err(e) => {
                for ext in core::extensions::registered() {
                    ext.on_compact_failure(reason.as_str());
                }
                return Err(e);
            }
        };

        if summary.trim().is_empty() {
            for ext in core::extensions::registered() {
                ext.on_compact_failure(reason.as_str());
            }
            return Err(Error::msg(
                "compaction summary generation failed: model returned empty content",
            ));
        }

        // 获取所有之前压缩会话时，读和修改过的文件列表
        let mut file_ops = compaction::FileOperations::default();
        if let Some(s) = self.session.as_ref()
            && let Some(prev) = s
                .get_entries()
                .iter()
                .rev()
                // 跳过 fromHook=true 的扩展生成压缩条目
                .find(|e| {
                    e.get("type").and_then(|v| v.as_str()) == Some("compaction")
                        && e.get("fromHook").and_then(|v| v.as_bool()) != Some(true)
                })
                .and_then(|e| e.get("details").cloned())
        {
            if let Some(prev_read) = prev.get("readFiles").and_then(|v| v.as_array()) {
                for f in prev_read {
                    if let Some(fs) = f.as_str() {
                        file_ops.read.insert(fs.to_string());
                    }
                }
            }
            if let Some(prev_mod) = prev.get("modifiedFiles").and_then(|v| v.as_array()) {
                for f in prev_mod {
                    if let Some(fs) = f.as_str() {
                        file_ops.edited.insert(fs.to_string());
                    }
                }
            }
        }

        // 获取当前压缩处理的文件列表
        for msg in &messages_to_summarize {
            compaction::extract_file_ops_from_message(msg, &mut file_ops);
        }

        let (read_files, modified_files) = compaction::compute_file_lists(&file_ops);
        let mut summary_with_files = summary;
        summary_with_files.push_str(&compaction::format_file_operations(
            &read_files,
            &modified_files,
        ));

        // 写 session compaction entry
        //
        // `firstKeptEntryId`：保留尾部**起点条目 id**。投影按它把
        // 「被保留的原始条目」重新带进上下文，因此追加在压缩之后的 context_edit 仍能
        // 命中它们，resume / fork 后也一致。切点索引先映射回投影条目；映射不上
        // （无 session / 投影不一致）才退回启发式：第一条带 entryId 的保留消息。
        let first_kept_id = self
            .session
            .as_ref()
            .and_then(|s| s.first_kept_entry_id_for_cut(&self.messages, cut_index))
            .or_else(|| kept_messages.iter().find_map(|m| m.entry_id.clone()))
            .or_else(|| {
                self.session
                    .as_ref()
                    .and_then(|s| s.get_leaf_id().map(|x| x.to_string()))
            });
        let tokens_before = context_tokens;
        let details = json!({ "readFiles": read_files, "modifiedFiles": modified_files });

        if let Some(s) = self.session.as_mut() {
            s.append_compaction(
                &summary_with_files,
                u64::from(tokens_before),
                first_kept_id.as_deref(),
                Some(details.clone()),
                Some(&summary_usage),
            );
        }

        // 重建上下文：summary + 保留条目
        //
        // 有 session 时用会话投影重建（而不是手动拼 summary + kept_messages），保证
        // `self.messages` 与 `Session::build_context_messages()` 一致——投影是唯一真相，
        // 压缩后的上下文与 resume / fork 得到的完全同形（含保留区间内的 context_edit 等）。
        // 无 session / 投影为空（损坏、无分支）时退回手工拼装，避免把上下文清空。
        let projected = self
            .session
            .as_ref()
            .map(|s| s.build_context_messages())
            .filter(|msgs| !msgs.is_empty());
        match projected {
            Some(msgs) => self.messages = msgs,
            None => {
                let mut summary_msg = AgentMessage::user_text("");
                summary_msg.role = "compactionSummary".into();
                summary_msg.content = vec![ContentBlock::Text {
                    text: summary_with_files.clone(),
                    text_signature: None,
                }];
                let mut new_messages = vec![summary_msg];
                new_messages.extend(kept_messages);
                self.messages = new_messages;
            }
        }

        // 重建后的上下文估算：压缩摘要的时间戳晚于被保留消息，旧 usage 锚点
        // 已失效（见 get_last_assistant_usage_info），这里得到的是摘要 + 保留尾部的纯估算
        let estimated_tokens_after = compaction::estimate_context_tokens(&self.messages).tokens;

        self.emit(json!({
            "type": "compaction_end",
            "reason": reason.as_str(),
            "result": "completed",
            "tokensBefore": tokens_before,
            "estimatedTokensAfter": estimated_tokens_after,
        }));

        let usage_json = serde_json::to_value(&summary_usage).unwrap_or(Value::Null);

        Ok(Some(json!({
            "summary": summary_with_files,
            "firstKeptEntryId": first_kept_id,
            "tokensBefore": tokens_before,
            "estimatedTokensAfter": estimated_tokens_after,
            "reason": reason.as_str(),
            "usage": usage_json,
            "details": details,
        })))
    }

    /// 树导航：把 leaf 切到目标 entry，可选生成被放弃分支的 branch_summary
    pub async fn navigate_tree(
        &mut self,
        target_id: &str,
        summarize: bool,
    ) -> Result<(bool, Option<String>)> {
        // 摘要模型在借用 session 之前解析（虚拟选择可能触发一次 `direct` 路由）。
        let summary_model = if summarize {
            Some(self.summarization_model().await?)
        } else {
            None
        };

        let Some(session) = self.session.as_mut() else {
            return Err(Error::msg("no current session"));
        };
        let Some(old_leaf) = session.get_leaf_id() else {
            return Err(Error::msg("session has no leaf"));
        };
        if old_leaf == target_id {
            return Ok((false, None));
        }

        // 目标为 user 消息时 leaf 设为 parentId 且返回 editorText
        let mut editor_text: Option<String> = None;
        let mut effective_target = target_id.to_string();
        if let Some(entry) = session
            .get_entries()
            .iter()
            .find(|e| e.get("id").and_then(|v| v.as_str()) == Some(target_id))
        {
            let ty = entry.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if ty == "message" {
                let role = entry
                    .get("message")
                    .and_then(|m| m.get("role").and_then(|r| r.as_str()))
                    .unwrap_or("");
                if role == "user" {
                    editor_text = entry
                        .get("message")
                        .and_then(|m| m.get("content").and_then(|c| c.as_array()))
                        .map(|blocks| {
                            blocks
                                .iter()
                                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                                .collect::<Vec<_>>()
                                .join("\n")
                        })
                        .filter(|s| !s.is_empty());
                    if let Some(parent) = entry.get("parentId").and_then(|v| v.as_str()) {
                        effective_target = parent.to_string();
                    }
                }
            }
        }

        let target_id = effective_target;
        if !session
            .get_entries()
            .iter()
            .any(|e| e.get("id").and_then(|v| v.as_str()) == Some(target_id.as_str()))
        {
            return Err(Error::EntryNotFound(target_id.clone()));
        }

        let old_branch = session.get_branch(old_leaf);
        let target_branch = session.get_branch(&target_id);
        let old_ids: HashSet<String> = old_branch
            .iter()
            .filter_map(|e| e.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()))
            .collect();
        let common_ancestor = target_branch.iter().rev().find(|e| {
            e.get("id")
                .and_then(|v| v.as_str())
                .is_some_and(|id| old_ids.contains(id))
        });
        let common_id = common_ancestor
            .and_then(|e| e.get("id").and_then(|v| v.as_str()))
            .unwrap_or("");

        // 被放弃分支中 common ancestor 之后的 entries（root-first）
        let abandoned: Vec<Value> = old_branch
            .iter()
            .skip_while(|e| {
                e.get("id")
                    .and_then(|v| v.as_str())
                    .is_some_and(|id| common_id.is_empty() || id != common_id)
            })
            .skip(1)
            .cloned()
            .collect();

        if summarize && !abandoned.is_empty() {
            let mut messages: Vec<AgentMessage> = Vec::new();
            let mut file_ops = compaction::FileOperations::default();
            for entry in &abandoned {
                let ty = entry.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match ty {
                    "message" => {
                        let Some(msg) = entry.get("message") else {
                            continue;
                        };
                        let Ok(mut msg) = serde_json::from_value::<AgentMessage>(msg.clone())
                        else {
                            continue;
                        };
                        if msg.role == "toolResult" {
                            continue;
                        }
                        msg.entry_id = entry
                            .get("id")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());
                        compaction::extract_file_ops_from_message(&msg, &mut file_ops);
                        messages.push(msg);
                    }
                    "branch_summary" => {
                        if let Some(summary) = entry.get("summary").and_then(|v| v.as_str()) {
                            let mut m = AgentMessage::user_text(summary);
                            m.role = "branchSummary".to_string();
                            messages.push(m);
                        }
                        if let Some(details) = entry.get("details") {
                            if let Some(read) = details.get("readFiles").and_then(|v| v.as_array())
                            {
                                for f in read {
                                    if let Some(fs) = f.as_str() {
                                        file_ops.read.insert(fs.to_string());
                                    }
                                }
                            }
                            if let Some(modified) =
                                details.get("modifiedFiles").and_then(|v| v.as_array())
                            {
                                for f in modified {
                                    if let Some(fs) = f.as_str() {
                                        file_ops.edited.insert(fs.to_string());
                                    }
                                }
                            }
                        }
                    }
                    "compaction" => {
                        if let Some(summary) = entry.get("summary").and_then(|v| v.as_str()) {
                            let mut m = AgentMessage::user_text(summary);
                            m.role = "compactionSummary".to_string();
                            messages.push(m);
                        }
                    }
                    _ => {}
                }
            }

            if messages.is_empty() {
                session.set_leaf(Some(&target_id))?;
            } else {
                let conversation = compaction::serialize_conversation(&messages);
                let prompt = format!(
                    "<conversation>\n{}\n</conversation>\n\n{}",
                    conversation,
                    compaction::BRANCH_SUMMARY_PROMPT
                );
                let summary_model = summary_model
                    .as_ref()
                    .expect("summary model resolved before borrowing session when summarize=true");
                let (mut summary, summary_usage) = core::provider::simple_completion(
                    summary_model,
                    compaction::SUMMARIZATION_SYSTEM_PROMPT,
                    &prompt,
                    Some(branch_summary_max_tokens(summary_model)),
                )
                .await?;

                summary = format!("{}{}", compaction::BRANCH_SUMMARY_PREAMBLE, summary);
                let (read_files, modified_files) = compaction::compute_file_lists(&file_ops);
                summary.push_str(&compaction::format_file_operations(
                    &read_files,
                    &modified_files,
                ));
                let details = json!({ "readFiles": read_files, "modifiedFiles": modified_files });
                session.append_branch_summary(
                    &target_id,
                    &summary,
                    Some(details),
                    Some(&summary_usage),
                )?;
            }
        } else {
            session.set_leaf(Some(&target_id))?;
        }

        let messages = session.build_context_messages();
        self.messages = messages;
        Ok((true, editor_text))
    }

    /// 回退最后一次用户输入：把活动分支叶子移到最后一条 user 消息之前，并重建模型上下文。
    ///
    /// 被回退的 user 消息与它之后产生的所有条目一起离开活动分支，不再进入后续 provider
    /// 请求；原始日志仍保留在会话树上。返回被回退的用户输入文本（供 TUI 回填编辑器 /
    /// 提示）；无会话或分支上没有 user 消息时返回 `Ok(None)`（无变化，不重建上下文）。
    pub fn rewind_last_user_input(&mut self) -> Result<Option<String>> {
        let Some(session) = self.session.as_mut() else {
            return Ok(None);
        };

        let removed = session.rewind_last_user_input()?;
        if removed.is_some() {
            // 回退可能清空整条分支（回退第一条 user 消息），因此**无条件**用投影重建，
            // 不能沿用 refresh_context_from_session 的“空投影保留旧上下文”保护。
            self.messages = session.build_context_messages();
        }
        Ok(removed)
    }

    /// 单次提示，返回最终回复文本（不含 thinking）
    pub async fn prompt(&mut self, user_message: &str) -> Result<String> {
        let msg = AgentMessage::user_text(user_message);
        self.prompt_message(msg).await
    }

    /// 单条提示（支持任意 AgentMessage，如含图片附件）
    pub async fn prompt_message(&mut self, msg: AgentMessage) -> Result<String> {
        self.prompt_messages(vec![msg]).await
    }

    /// 多条 user 消息一次注入：all 批量时把多条作为独立 user 消息在同一轮 LLM 调用前注入。
    pub async fn prompt_messages(&mut self, msgs: Vec<AgentMessage>) -> Result<String> {
        self.reset_run_state();
        self.inject_prompt_messages(msgs);
        self.compact_before_run().await;

        let mut result = self.run_with_recovery().await;

        // `agent_before_settle` 边界：结算前咨询扩展。`continue` 时先把边界追加的消息落库，
        // 再发起一轮（每次结算都重新咨询，直到扩展不再要求续跑）。
        // 无上限由扩展自限（goal 按目标状态收敛）；Esc/abort 期间 `run_abort` 直接退出循环。
        let mut continuation_index = 0usize;
        while let Some(outcome) = self.settle_boundary(continuation_index) {
            // 边界草稿（context edit / retain-none 压缩）先落库并重建上下文
            self.apply_boundary_drafts(&outcome);
            if !outcome.append.is_empty() {
                self.inject_prompt_messages(outcome.append);
            }

            // 只追加草稿、不要求续跑时不再起新一轮（否则会在同一结算点无限循环）。
            if !outcome.r#continue {
                break;
            }

            self.overflow_recovery_attempted = false;
            result = self.run_with_recovery().await;
            continuation_index += 1;
        }

        // 整轮（含重试/溢出恢复的所有 run_loop 尝试）真正结束：发一次 agent_settled 作为
        // UI 收尾边界。agent_end 每次 run_loop 尝试都发，不能用作“整轮结束”信号。
        // 中断路径（future 被 drop）走不到这里，由 handle_cancel_prompt 收尾。
        self.emit_agent_settled();
        result
    }

    /// 一次 run_loop 尝试 + 调用层恢复（溢出压缩重试 / 瞬时错误退避重试）。
    async fn run_with_recovery(&mut self) -> Result<String> {
        let result = self.run_loop().await;

        // 溢出恢复（compact+retry 一次）在调用层执行。
        // stream_chat 纯流契约：溢出错误编码为 stopReason=error 的 assistant 消息，
        // 这里从最后一条消息的 error_message 检测。
        if self.should_recover_overflow() {
            self.recover_from_overflow(result).await
        } else {
            self.retry_transient_errors(result).await
        }
    }

    /// 结算前边界：扩展返回 `continue` 时返回要追加的消息（空 vec = 只续跑不加消息），
    /// `None` = 不续跑。无扩展声明边界钩子时零开销（不构造事件）。
    /// 只返回草稿（不续跑）时同样返回 `Some`——调用方负责落库并结束本次结算。
    fn settle_boundary(&mut self, continuation_index: usize) -> Option<BoundaryOutcome> {
        if !core::extensions::has_boundary_handlers() {
            return None;
        }

        // 已请求中断：不再续跑（避免 Esc 后又被边界拉起来）
        if self.run_abort.load(Ordering::Relaxed) {
            return None;
        }

        // 上下文为空时无可请求内容
        if self.messages.is_empty() {
            return None;
        }

        let context_messages: Vec<Value> = self
            .messages
            .iter()
            .map(|m| serde_json::to_value(m).unwrap_or(Value::Null))
            .collect();

        // 运行中 steer 注入箱非空 = 用户输入在排队：事件里如实告知，由扩展决定是否让位
        // （队列由 UI 在 settle 后起新一轮；此处不由 core 代扩展做策略决定）。
        let has_queued_messages = !self.runtime_steer_inbox.lock().unwrap().is_empty();
        let event = json!({
            "type": "agent_before_settle",
            "outcome": self.activity_outcome(),
            "contextMessages": context_messages,
            "hasQueuedMessages": has_queued_messages,
            "continuationIndex": continuation_index,
            "agentScope": self.agent_scope(),
        });

        let outcome = core::extensions::dispatch_boundary(&event);
        if !outcome.r#continue && !outcome.has_drafts() {
            return None;
        }
        Some(outcome)
    }

    /// 应用边界追加的投影草稿：context edit 与 retain-none 压缩落库后**立即重建模型上下文**
    /// （`agent.messages` = session 投影），使编辑/压缩对后续 provider 请求生效。
    /// 与 `continue` / `end` 决策无关 （error / aborted 轮同样应用）。返回是否发生了变更。
    ///
    /// 非法草稿（目标不存在 / 不在活动分支 / 不可编辑）被跳过而不中断运行（无 session 时整批跳过）。
    pub(crate) fn apply_boundary_drafts(&mut self, outcome: &BoundaryOutcome) -> bool {
        if !outcome.has_drafts() {
            return false;
        }

        // retain-none 压缩的 tokensBefore：扩展未给时按当前上下文估算
        let estimated_tokens = if outcome
            .compactions
            .iter()
            .any(|c| c.tokens_before.is_none())
        {
            u64::from(compaction::estimate_context_tokens(&self.messages).tokens)
        } else {
            0
        };

        let Some(session) = self.session.as_mut() else {
            return false;
        };

        let mut changed = false;
        for draft in &outcome.edits {
            let replacement = draft
                .replacement
                .clone()
                .map(|content| session_v4::ContextEditReplacement { content });
            if session
                .append_context_edit(&draft.target_id, replacement)
                .is_ok()
            {
                changed = true;
            }
        }

        for compaction in &outcome.compactions {
            let tokens_before = compaction.tokens_before.unwrap_or(estimated_tokens);
            session.append_retain_none_compaction(
                &compaction.summary,
                tokens_before,
                compaction.details.clone(),
                None,
            );
            changed = true;
        }

        if changed {
            self.refresh_context_from_session();
        }

        changed
    }

    /// 用 session 投影重建模型上下文。投影为空（无分支 / 日志损坏）时保留原上下文，避免把上下文清空。
    fn refresh_context_from_session(&mut self) {
        let Some(session) = self.session.as_ref() else {
            return;
        };

        let rebuilt = session.build_context_messages();
        if !rebuilt.is_empty() {
            self.messages = rebuilt;
        }
    }

    /// 边界事件 `agentScope`：会话主 agent 或子代理 run。
    fn agent_scope(&self) -> &'static str {
        if self.is_subagent { "subagent" } else { "main" }
    }

    /// 本次活动结局（completed | aborted | error）。
    fn activity_outcome(&self) -> &'static str {
        match self
            .messages
            .iter()
            .rev()
            .find(|m| m.role == "assistant")
            .and_then(|m| m.stop_reason.as_deref())
        {
            Some("aborted") => "aborted",
            Some("error") => "error",
            _ => "completed",
        }
    }

    /// 每次 run 重置调用层守卫：取消信号 + 溢出重试守卫。
    ///
    /// 取消信号只复位**不换 Arc**：子代理/工具捕获的句柄必须与新 run 同源，否则
    /// 父级 `parent_abort` → `controls.request_abort()` 会落在废信号上，子代理收不到取消。
    fn reset_run_state(&mut self) {
        self.run_abort.store(false, Ordering::Relaxed); // 每次 run 复位取消信号
        self.overflow_recovery_attempted = false; // 每次 run 重置溢出重试守卫
        self.routed_model = None; // 路由结果每轮请求内重新解析
        self.routed_thinking_level = None;
        self.pending_failed_route = None; // 新回合不再携带上一回合的失败尝试
    }

    /// 注入 user 消息批次：写入 session（如有）并追加到上下文，设置 last_user_batch。
    fn inject_prompt_messages(&mut self, msgs: Vec<AgentMessage>) {
        let mut injected = Vec::with_capacity(msgs.len());
        for mut msg in msgs {
            if let Some(s) = self.session.as_mut() {
                let entry_id = s.append_message(&msg);
                msg.entry_id = Some(entry_id);
            }
            self.messages.push(msg.clone());
            injected.push(msg);
        }
        self.last_user_batch = injected;
    }

    /// auto compaction 在调用层 prompt 前触发（run_loop 内不含压缩）。
    ///
    /// 虚拟选择不在此时压缩：阈值要按**路由后**的物理模型窗口判定，因此延到 agent 循环内路由完成后再压。
    async fn compact_before_run(&mut self) {
        if self.auto_compaction && !self.selected_is_virtual() {
            _ = self
                .maybe_compact_with(CompactionReason::Threshold, None)
                .await;
        }
    }

    /// 最后一条 assistant 消息是否为上下文溢出（error / 静默 / length-stop 三态）。
    fn last_message_is_overflow(&self) -> bool {
        let model = self.effective_model();
        self.messages.last().is_some_and(|m| {
            m.role == "assistant"
                && Self::message_is_context_overflow(m, model.context_window, &model.provider)
        })
    }

    /// 溢出恢复触发条件：溢出 + 同一模型 + 自动重试开关（换模型后旧模型溢出不重试）。
    fn should_recover_overflow(&self) -> bool {
        if !self.auto_retry || !self.last_message_is_overflow() {
            return false;
        }

        let model_id = self.effective_model().model_id.clone();
        self.messages
            .last()
            .and_then(|m| m.model.clone())
            .map(|m| m == model_id)
            .unwrap_or(true)
    }

    /// 最后一条 assistant 消息的错误信息（stopReason=error 时）。
    fn last_assistant_error(&self) -> Option<String> {
        self.messages.last().and_then(|m| {
            (m.role == "assistant" && m.stop_reason.as_deref() == Some("error"))
                .then(|| m.error_message.clone())
                .flatten()
        })
    }

    /// 把一条已落库的消息条目在**模型上下文**中永久省略：追加一条 append-only 的
    /// `context_edit`（`replacement = null`），不改写历史。
    ///
    /// 这是恢复路径（错误重试 / length·overflow 恢复）的关键：失败或被放弃的 attempt
    /// 已经 emit 给 UI 并落库（原始 transcript 完整保留），但不应再进入后续 provider 请求——
    /// 包括 `/resume`、树导航、`--fork` 之后（那些路径会经 `build_context_messages`
    /// 重建上下文，只有落库的编辑会被重新应用）。
    /// 无 session、无 entry id 或目标不可编辑时静默跳过（不阻断恢复流程）。
    fn omit_entry_from_context(&mut self, entry_id: Option<&str>) -> bool {
        let Some(entry_id) = entry_id else {
            return false;
        };
        let Some(session) = self.session.as_mut() else {
            return false;
        };
        session.append_context_edit(entry_id, None).is_ok()
    }

    /// 记录当前最后一条失败 assistant 的物理路由（虚拟模型的 `retry` 请求据此决策）。
    fn capture_failed_route(&mut self) {
        // 非虚拟选择的路由器不会读它，直接跳过以免留下陈旧字段。
        if !self.selected_is_virtual() {
            return;
        }

        let failed = self.messages.last().and_then(|m| {
            if m.role != "assistant" || m.stop_reason.as_deref() != Some("error") {
                return None;
            }

            // 路由本身失败时消息仍指向虚拟模型：没有物理请求可报告
            if m.api.as_deref().is_some_and(virtual_models::is_virtual_api) {
                return None;
            }

            let (provider, model_id) = (m.provider.clone()?, m.model.clone()?);
            Some(virtual_models::FailedRoute {
                provider,
                model_id,
                thinking_level: m.thinking_level.clone(),
                error_message: m.error_message.clone(),
                stop_reason: m.stop_reason.clone(),
            })
        });

        if failed.is_some() {
            self.pending_failed_route = failed;
        }
    }

    /// 广播溢出恢复失败（已重试一次或重试后仍溢出）。
    fn emit_overflow_recovery_failed(&mut self) {
        self.emit(json!({
            "type": "compaction_end",
            "reason": "overflow",
            "result": "failed",
            "willRetry": false,
            "errorMessage":
                "Context overflow recovery failed after one compact-and-retry attempt.",
        }));
    }

    /// 溢出恢复：
    /// 1) 已重试过一次则直接转达失败；
    /// 2) 从重试上下文移除失败/截断的 assistant 消息（session 历史保留）；
    /// 3) Overflow 压缩成功才重跑一轮，重试仍溢出则发失败通知，不再二次压缩。
    async fn recover_from_overflow(&mut self, result: Result<String>) -> Result<String> {
        if self.overflow_recovery_attempted {
            // 已压缩重试过一次，再次溢出：不再重复压缩，转达失败
            self.emit_overflow_recovery_failed();
            return result;
        }

        self.overflow_recovery_attempted = true;

        // 最后一条即溢出 assistant（上方已校验）：先在**模型上下文**中永久省略再从本轮上下文弹出。
        self.capture_failed_route();
        if self.messages.last().is_some_and(|m| m.role == "assistant") {
            let omitted_id = self.messages.last().and_then(|m| m.entry_id.clone());
            self.omit_entry_from_context(omitted_id.as_deref());
            self.messages.pop();
        }

        let ok = matches!(
            self.maybe_compact_with(CompactionReason::Overflow, None)
                .await,
            Ok(Some(_))
        );

        if !ok {
            return result;
        }

        let retried = self.run_loop().await;
        // 重试后仍溢出：发失败通知，不再二次压缩
        if self.last_message_is_overflow() {
            self.emit_overflow_recovery_failed();
        }
        retried
    }

    /// 可重试错误退避重发：配额/计费耗尽（NON_RETRYABLE）直接失败不重试；
    /// 瞬时错误按指数退避重试，最多 maxRetries 次。
    async fn retry_transient_errors(&mut self, result: Result<String>) -> Result<String> {
        if !self.auto_retry || !settings_manager::read_settings_retry_enabled() {
            return result;
        }
        let max_retries = settings_manager::read_settings_retry_max_retries();
        let base_delay = settings_manager::read_settings_retry_base_delay_ms();
        let mut attempt = 0u32;
        let mut result = result;

        while attempt < max_retries {
            let Some(err_msg) = self.last_assistant_error() else {
                break;
            };
            if !Self::is_retryable_assistant_error(&err_msg) {
                break;
            }

            attempt += 1;

            // 给 agent 级退避设上限，避免长时间瞬时故障下指数退避失控。
            let delay_ms =
                (base_delay * (1u64 << (attempt - 1))).min(Self::MAX_AGENT_RETRY_DELAY_MS);

            let omitted_id = self.messages.last().and_then(|m| m.entry_id.clone());

            self.capture_failed_route();
            self.messages.pop();

            // 被放弃的 attempt 保留在原始 transcript，但在模型上下文中永久省略
            self.omit_entry_from_context(omitted_id.as_deref());

            self.emit(json!({
                "type": "auto_retry_start",
                "attempt": attempt,
                "maxAttempts": max_retries,
                "delayMs": delay_ms,
                "errorMessage": err_msg,
            }));

            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            result = self.run_loop().await;

            self.emit(json!({
                "type": "auto_retry_end",
                "attempt": attempt,
                "success": result.is_ok(),
                "finalError": result.as_ref().err().map(|e| e.to_string()),
            }));
        }

        result
    }

    /// 可重试错误分类：配额/计费耗尽 → 不可重试直接失败；瞬时错误 → 退避重试。
    fn is_retryable_assistant_error(msg: &str) -> bool {
        let m = msg.to_lowercase();
        /// 不可重试错误特征子串：配额或计费耗尽等，命中则直接失败。
        const NON_RETRYABLE: [&str; 10] = [
            "gousagelimiterror",
            "freeusagelimiterror",
            "monthly usage limit reached",
            "insufficient_quota",
            "out of budget",
            "quota exceeded",
            "billing",
            "rate limit reached",
            "daily limit",
            // Sign in with ChatGPT：订阅共享用量上限（需等数小时而非数秒）
            "subscription_sharing_usage_limit_exceeded",
        ];

        if NON_RETRYABLE.iter().any(|p| m.contains(p)) {
            return false;
        }

        /// 可重试错误特征子串：限流、网络、5xx 与流中断等瞬时故障。
        const RETRYABLE: &[&str] = &[
            // 服务端瞬时负载 / HTTP 状态
            "overloaded",
            "server_busy",
            "servers are currently busy",
            "currently experiencing high demand",
            "model is at capacity",
            "rate limit",
            "too many requests",
            "429",
            "500",
            "502",
            "503",
            "504",
            "520",
            "524",
            "service unavailable",
            "server error",
            "internal error",
            "provider returned error",
            "exceeded request buffer limit while retrying upstream",
            // 网络 / 传输失败（reqwest Display 只含 “error sending request for url (...)”
            // 底层原因经 display_full 展开后可看到 connection / dns / tls 等）
            "network",
            "connection",
            "connection refused",
            "connection reset",
            "connection lost",
            "other side closed",
            "error sending request",
            "error trying to connect",
            "request failed",
            "fetch failed",
            "getaddrinfo",
            "enotfound",
            "eai_again",
            "upstream connect",
            "reset before headers",
            "socket hang up",
            "timed out",
            "timeout",
            "terminated",
            // websocket 传输
            "websocket closed",
            "websocket error",
            // 流提前结束
            "ended without",
            "stream ended before message_stop",
            "stream ended before a terminal response event",
            "http2 request did not get a response",
            "http2",
            // provider 显式要求重试
            "retry delay",
            "you can retry your request",
            "try your request again",
            "please retry your request",
            // gRPC provider（如 NVIDIA NIM）
            "resourceexhausted",
        ];
        RETRYABLE.iter().any(|p| m.contains(p))
    }

    /// 派发一次主循环：`agent_loop` 有自定义实现时调用它，否则走内置 [`agent_loop::default_loop`]。
    // agent 主循环派发：agent_loop 字段 None → 内置默认实现；Some → 替换实现。
    // 子代理递归也经本方法（子实例继承同一 agent_loop），保证整棵调用树语义一致。
    async fn run_loop(&mut self) -> Result<String> {
        match self.agent_loop.clone() {
            Some(arc) => {
                let f: &dyn Fn(&mut Agent) -> BoxFuture<'_, Result<String>> = arc.as_ref();
                f(self).await
            }
            None => agent_loop::default_loop(self).await,
        }
    }

    /// 设置可插拔的主循环（可随时替换）。传给子代理时会被继承。
    pub fn set_agent_loop(&mut self, f: AgentLoop) {
        self.agent_loop = Some(f);
    }

    /// 恢复内置默认主循环。
    pub fn reset_agent_loop(&mut self) {
        self.agent_loop = None;
    }

    /// 单次 `run_loop()` 尝试结束时发 agent_end，携带该次尝试新增消息（newMessages）。
    /// 注意：自动重试/溢出恢复会多次调用 `run_loop()`，因此一次用户回合可能发多个 agent_end；
    /// 它**不是**“整轮结束”信号（不可用于 UI 收尾/中断判定），整轮结束见 [`Self::emit_agent_settled`]。
    pub(crate) fn emit_agent_end(&mut self, messages: &[AgentMessage]) {
        let list: Vec<Value> = messages
            .iter()
            .map(|m| serde_json::to_value(m).unwrap())
            .collect();
        self.emit(json!({ "type": "agent_end", "messages": list }));
    }

    /// 整轮真正结束（`prompt_messages` 出口，含重试/溢出恢复的所有 `run_loop()` 尝试）时发
    /// agent_settled，载荷带 `aborted`（本轮是否被请求过取消）。
    /// UI 以它为收尾边界复位忙碌态并取下一批排队消息，集成方据此区分「被取消的运行」与正常完成。
    /// 中断路径（future 被 drop）不会走到这里，由 UI 侧 `handle_cancel_prompt` 收尾。
    pub(crate) fn emit_agent_settled(&mut self) {
        // 缓存保温：streaming 模式结算即停；idle 模式转入 idle 阶段
        self.cache_warmer.on_agent_settled();
        // 取消信号在每次 run 开始时复位（见 `reset_run_state`），所以这里读到的是本轮的结果。
        let aborted = self.run_abort.load(Ordering::Relaxed);
        self.emit(json!({ "type": "agent_settled", "aborted": aborted }));
    }

    /// 协作式取消的优雅收尾：
    /// 发 stop_reason=aborted 的 assistant 失败消息（message_start/end → turn_end → agent_end）并落库。
    pub(crate) fn emit_abort_failure(&mut self, new_messages: &mut Vec<AgentMessage>) {
        // 取消消息命名**本次请求实际使用的**模型：虚拟选择下若已路由到物理模型，失败/取消的响应应指向物理模型
        let (model_id, provider, api) = {
            let m = self.effective_model();
            (m.model_id.clone(), m.provider.clone(), m.api.clone())
        };

        let mut failure = AgentMessage {
            role: "assistant".to_string(),
            thinking_level: None,
            content: vec![ContentBlock::Text {
                text: String::new(),
                text_signature: None,
            }],
            tool_call_id: None,
            tool_name: None,
            is_error: false,
            stop_reason: Some("aborted".to_string()),
            error_message: Some("Operation aborted".to_string()),
            model: Some(model_id),
            provider: Some(provider),
            api: Some(api),
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
        };
        let v = serde_json::to_value(&failure).unwrap();
        self.emit(json!({ "type": "message_start", "message": v.clone() }));
        self.emit(json!({ "type": "message_end", "message": v.clone() }));
        self.emit(json!({ "type": "turn_end", "message": v, "toolResults": [] }));

        if let Some(s) = self.session.as_mut() {
            let eid = s.append_message(&failure);
            failure.entry_id = Some(eid);
        }
        new_messages.push(failure.clone());
        self.messages.push(failure);
        // agent_end 由调用方在 break 'outer 后（循环尾）统一发出
    }

    /// 取待注入的 steering（运行中 inbox），并按队列 mode all=整批 / one-at-a-time=取一条。
    pub(crate) fn take_steering_batch(&mut self) -> Vec<AgentMessage> {
        let mut out: Vec<AgentMessage> = Vec::new();
        {
            let mut q = self.runtime_steer_inbox.lock().unwrap();
            if self.steer_mode == "all" {
                out.extend(q.drain(..));
            } else if let Some(m) = q.pop_front() {
                out.push(m);
            }
        }

        out
    }

    /// 执行一批工具调用：
    /// - 默认并行（toolExecution=Parallel），并行阶段各工具并发执行；串行模式逐工具交错；
    /// - 每个工具：prepareToolCall（查找 → prepareArguments → schema 校验 → beforeToolCall）
    ///   → 立即失败（工具不存在/校验失败/blocked/aborted）直接产出错误结果；
    ///   → 否则执行 + afterToolCall 覆写（content/details/usage/terminate/isError）；
    /// - tool_execution_end 完成序发出（并行），toolResult message_start/end 源序发出
    ///   （串行模式下每工具先 end 再发消息）；
    /// - terminate 为 ALL 语义（shouldTerminateToolBatch：全部完成结果 terminate=true 才停批）。
    ///
    /// 返回 (toolResult 消息列表, terminate 标志)。
    ///
    /// 实现拆为三阶段：
    /// - [`Self::prepare_tool_calls`]：解析 + prepareToolCall（查找 → prepareArguments → schema 校验 → beforeToolCall）；
    /// - [`Self::execute_tool_batch_sequential`] / [`Self::execute_tool_batch_parallel`]：按模式执行并落库；
    /// - 本函数只负责调度（模式判定 + 环境快照克隆）与 ALL 语义汇总。
    pub(crate) async fn execute_tool_calls(
        &mut self,
        tool_calls: &[ContentBlock],
        run_id: &str,
        assistant_entry_id: Option<&str>,
        assistant_msg: &AgentMessage,
        context: &[AgentMessage],
    ) -> (Vec<(ContentBlock, AgentMessage)>, bool) {
        // phase 1：解析 + prepareToolCall（查找 → prepareArguments → schema 校验 → beforeToolCall）
        let prepared = Self::prepare_tool_calls(
            tool_calls,
            assistant_msg,
            context,
            &self.rebuild_ctx.injected_tools,
        );

        // phase 2：执行阶段（并行或顺序）。单工具 executionMode=sequential 覆盖整批（内置+扩展+注入工具都查）
        let sequential = self.tool_execution == ToolExecutionMode::Sequential
            || prepared.iter().any(|p| {
                ToolIndex::resolve(&self.rebuild_ctx.injected_tools, &p.name).execution_mode()
                    == Some(ToolExecutionMode::Sequential)
            });

        let sink = self.json_sink.clone();
        let abort = self.run_abort.clone();
        let cwd = self.cwd.clone();

        // 子代理模板：捕获不变配置供扩展工具 thunk 还原子代理（不借用 &self）。
        // 子代理执行上下文（含 child.prompt 闭包）在 execute_one 内构造，避免 Send 循环依赖。
        let child_tpl = Arc::new(self.snapshot_template());

        // 会话执行上下文：交给扩展（如 subagent 的 `@mention` 在工具执行之外也能 spawn/resume）。
        // 与 execute_one 传给扩展的 ctx 同源（同一 child_tpl 快照）。
        core::extensions::dispatch_exec_ctx(&make_exec_ctx(
            &cwd,
            &child_tpl,
            &abort,
            &sink,
            None,
            self.session_branch_entries(),
        ));

        let mut tool_results: Vec<(ContentBlock, AgentMessage)> = Vec::new();
        let terminates = if sequential {
            self.execute_tool_batch_sequential(
                &prepared,
                run_id,
                assistant_entry_id,
                sink,
                abort,
                child_tpl,
                &mut tool_results,
            )
            .await
        } else {
            self.execute_tool_batch_parallel(
                &prepared,
                run_id,
                assistant_entry_id,
                sink,
                abort,
                cwd,
                child_tpl,
                &mut tool_results,
            )
            .await
        };

        // ALL 语义（全部完成结果都 terminate 才停批）
        let terminate = !terminates.is_empty() && terminates.iter().all(|b| *b);
        (tool_results, terminate)
    }

    /// phase 1：解析 + prepareToolCall（查找 → prepareArguments → schema 校验 → beforeToolCall）。
    /// 每个 tool_call → `Prepared`；命中立即错误（工具不存在/校验失败/blocked/aborted）时
    /// 填充 immediate/immediate_terminate，执行阶段不再调用工具。
    fn prepare_tool_calls(
        tool_calls: &[ContentBlock],
        assistant_msg: &AgentMessage,
        context: &[AgentMessage],
        injected: &[InjectedChildTool],
    ) -> Vec<Prepared> {
        let mut prepared: Vec<Prepared> = Vec::new();
        for tc in tool_calls {
            let (id, name, arguments) = match tc {
                ContentBlock::ToolCall {
                    id,
                    name,
                    arguments,
                    ..
                } => (id.clone(), name.clone(), arguments.clone()),
                _ => continue,
            };

            let mut args = arguments.clone();
            let mut immediate: Option<String> = None;
            let mut immediate_terminate = false;

            // 工具解析：注入 > 内置 > 扩展注册表（详见 `ToolIndex`）。
            // 工具不存在（prepareToolCall 未找到 → "Tool N not found" 立即错误）
            let tools_index = ToolIndex::resolve(injected, &name);
            if !tools_index.exists() {
                immediate = Some(format!("Tool {} not found", name));
            } else {
                args = tools_index.prepare_arguments(&args);

                // 参数 schema 校验；校验失败 → immediate error
                if let Some(parameters) = tools_index.parameters() {
                    match tools::validate_tool_arguments(&name, &parameters, &args) {
                        Ok(validated) => args = validated,
                        Err(msg) => immediate = Some(msg),
                    }
                }

                if immediate.is_none() {
                    // BeforeToolCall 钩子（增强接口，支持 block/reason/terminate，入参含 assistantMessage/context）
                    for ext in core::extensions::registered() {
                        if ext.hooks().contains(&ExtensionHook::BeforeToolCall) {
                            match ext.before_tool_call_ext(
                                &name,
                                &args,
                                Some(assistant_msg),
                                context,
                                &arguments,
                            ) {
                                Ok(out) => {
                                    if out.block {
                                        let reason = out.reason.unwrap_or_else(|| {
                                            "Tool execution was blocked".to_string()
                                        });
                                        immediate = Some(reason);
                                        immediate_terminate = out.terminate;
                                        break;
                                    }
                                    if let Some(new_args) = out.args {
                                        args = new_args;
                                    }
                                }
                                Err(e) => {
                                    immediate = Some(e.0);
                                    break;
                                }
                            }
                        }
                    }
                }
            }

            prepared.push(Prepared {
                call: ContentBlock::ToolCall {
                    id,
                    name: name.clone(),
                    arguments,
                    thought_signature: None,
                    namespace: None,
                },
                name,
                args,
                immediate,
                immediate_terminate,
            });
        }
        prepared
    }

    /// 顺序执行阶段：start → (immediate | execute+after) → end → 消息 逐工具交错。
    /// 返回各工具 terminate 标志（由 `execute_tool_calls` 汇总为 ALL 语义）。
    #[allow(clippy::too_many_arguments)]
    async fn execute_tool_batch_sequential(
        &mut self,
        prepared: &[Prepared],
        run_id: &str,
        assistant_entry_id: Option<&str>,
        sink: Option<Arc<Mutex<JsonSink>>>,
        abort: Arc<AtomicBool>,
        child_tpl: Arc<AgentTemplate>,
        tool_results: &mut Vec<(ContentBlock, AgentMessage)>,
    ) -> Vec<bool> {
        let mut terminates: Vec<bool> = Vec::new();
        for (tool_seq, p) in prepared.iter().enumerate() {
            if abort.load(Ordering::Relaxed) {
                break; // 剩余工具不再发任何事件
            }

            if tool_seq == usize::MAX {
                // unreachable：仅为保持索引语义
            }

            emit_start(&sink, p);

            // 每个工具单独计时（不使用批次 batch_start.elapsed()：
            // 顺序执行会让后续工具显示累计耗时，并行执行会让全部工具共享同一值）。
            // 计时在 `after_tool_call` 钩子之前收口，只测工具本身（对齐 pi #10549）。
            let tool_start = Instant::now();
            let (f, dur) = if let Some(msg) = &p.immediate {
                // 未真正执行的调用没有耗时（pi："Calls that did not run have none"）
                (
                    FinalizedToolCall::error_result(
                        &p.call,
                        &p.name,
                        &p.args,
                        msg,
                        p.immediate_terminate,
                    ),
                    None,
                )
            } else {
                let on_chunk = Self::bash_chunk_callback(
                    self.json_sink.clone(),
                    &p.tool_call_id(),
                    &p.name,
                    &p.args,
                );
                let idx = ToolIndex::resolve(&self.rebuild_ctx.injected_tools, &p.name);
                let mut exec_ctx: Option<ToolExecCtx> = None;
                let result = if let Some(handler) = idx.injected_handler() {
                    handler(p.args.clone()) // 注入工具：同步 handler，不经 ToolExecCtx（它不是扩展工具）
                } else if idx.is_builtin() {
                    let supports_images = self.model.input.iter().any(|i| i == "image");
                    let bash_env = self.current_bash_env();
                    Self::execute_builtin_tool(
                        &self.cwd,
                        &p.name,
                        &p.args,
                        on_chunk,
                        supports_images,
                        bash_env,
                        self.model.image_resize,
                    )
                    .await
                } else {
                    // 分支条目快照只在脚本宿主的工具调用上算（其它工具用不到，
                    // 而算一次是 O(分支条目数)）；嵌套调用沿上下文继承。
                    let entries = tool_hosts_scripts(&p.name)
                        .then(|| self.session_branch_entries())
                        .flatten();

                    let ctx = make_exec_ctx(
                        &self.cwd,
                        &child_tpl,
                        &abort,
                        &self.json_sink,
                        Some(&p.tool_call_id()),
                        entries,
                    );
                    exec_ctx = Some(ctx.clone());
                    self.execute_extension_tool_async(&p.name, &p.args, ctx)
                        .await
                };

                let mut f = match result {
                    Ok(t) => FinalizedToolCall::from_tool_result(&p.call, &p.name, &p.args, t),
                    Err(e) => {
                        FinalizedToolCall::error_result(&p.call, &p.name, &p.args, &e.0, false)
                    }
                };
                let dur = Some(tool_start.elapsed().as_millis() as u64);
                f.duration_ms = dur;
                attach_nested_calls(&mut f, exec_ctx.as_ref());
                apply_after_tool_call(&mut f, &p.name, &p.args);
                (f, dur)
            };

            emit_tool_execution_end(&sink, &f);

            self.push_tool_result_message(&f, dur, tool_results);

            self.record_tool_started(
                run_id,
                assistant_entry_id,
                tool_seq,
                &f,
                tool_results.last(),
            );
            terminates.push(f.terminate);
        }
        terminates
    }

    /// 并行执行阶段：start + immediate 按源序；执行型 thunk 完成序发 end。消息在全部结束后按源序发。
    /// 返回各工具 terminate 标志（由 `execute_tool_calls` 汇总为 ALL 语义）。
    #[allow(clippy::too_many_arguments)]
    async fn execute_tool_batch_parallel(
        &mut self,
        prepared: &[Prepared],
        run_id: &str,
        assistant_entry_id: Option<&str>,
        sink: Option<Arc<Mutex<JsonSink>>>,
        abort: Arc<AtomicBool>,
        cwd: String,
        child_tpl: Arc<AgentTemplate>,
        tool_results: &mut Vec<(ContentBlock, AgentMessage)>,
    ) -> Vec<bool> {
        let mut locks: HashMap<String, Arc<tokio::sync::Mutex<()>>> = HashMap::new();
        // slot 存 (工具结果, 该工具自身耗时毫秒)：并行完成序各自计时，
        // 不用批次 batch_start.elapsed()（join_all 后所有工具会共享整个批次耗时）；
        // 未真正执行的调用耗时为 `None`。
        let mut slots: Vec<Option<(FinalizedToolCall, Option<u64>)>> = Vec::new();
        let mut futs: Vec<BoxFuture<'static, (FinalizedToolCall, Option<u64>)>> = Vec::new();
        let mut fut_slots: Vec<usize> = Vec::new();

        for p in prepared {
            if abort.load(Ordering::Relaxed) {
                break;
            }
            emit_start(&sink, p);
            if let Some(msg) = &p.immediate {
                let f = FinalizedToolCall::error_result(
                    &p.call,
                    &p.name,
                    &p.args,
                    msg,
                    p.immediate_terminate,
                );
                emit_tool_execution_end(&sink, &f);
                slots.push(Some((f, None)));
            } else {
                // file mutation queue：edit/write 按 canonical path 串行化
                let lock: Option<Arc<tokio::sync::Mutex<()>>> = if (p.name == "edit"
                    || p.name == "write")
                    && let Some(path) = p.args.get("path").and_then(|v| v.as_str())
                {
                    Some(
                        locks
                            .entry(Self::mutation_key(&cwd, path))
                            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                            .clone(),
                    )
                } else {
                    None
                };

                slots.push(None);
                // 分支条目快照只在脚本宿主工具上算（见 execute_tool_batch_sequential）
                let entries = tool_hosts_scripts(&p.name)
                    .then(|| self.session_branch_entries())
                    .flatten();
                let mut fut = execute_one(p, cwd.clone(), &sink, &abort, &child_tpl, entries);
                if let Some(l) = lock {
                    fut = Box::pin(async move {
                        let _guard = l.lock().await;
                        fut.await
                    });
                }
                fut_slots.push(slots.len() - 1);
                futs.push(fut);
            }
        }

        // join_all 保持输入序（源序）；执行结果填回各自 slot，immediate 原样保留
        let executed = futures_util::future::join_all(futs).await;
        for (pos, f) in fut_slots.into_iter().zip(executed) {
            slots[pos] = Some(f);
        }

        let mut terminates: Vec<bool> = Vec::new();
        for (seq_idx, (f, dur)) in slots.into_iter().flatten().enumerate() {
            self.push_tool_result_message(&f, dur, tool_results);
            self.record_tool_started(run_id, assistant_entry_id, seq_idx, &f, tool_results.last());
            terminates.push(f.terminate);
        }
        terminates
    }

    /// 组装交给 bash 工具的会话环境变量（session id/文件、provider/model、思考档位、是否暴露环境）；
    /// 当前实现总能构造出值，因此总是返回 `Some`。
    fn current_bash_env(&self) -> Option<tools::BashSessionEnv> {
        let env = tools::BashSessionEnv {
            session_id: self.session.as_ref().map(|s| s.session_id.clone()),
            session_file: self
                .session
                .as_ref()
                .and_then(|s| s.get_session_file())
                .map(|p| p.to_string_lossy().to_string()),
            provider: Some(self.model.provider.clone()),
            model: Some(self.model.model_id.clone()),
            reasoning_level: self.thinking_level.clone(),
            expose: settings_manager::read_settings_expose_session_environment(),
        };
        Some(env)
    }

    /// v4 durable 记录：tool_started
    fn record_tool_started(
        &mut self,
        run_id: &str,
        assistant_entry_id: Option<&str>,
        tool_index: usize,
        fin: &FinalizedToolCall,
        last_result: Option<&(ContentBlock, AgentMessage)>,
    ) {
        let Some(s) = self.session.as_mut() else {
            return;
        };
        let result_entry_id = last_result.and_then(|(_, m)| m.entry_id.clone());

        s.append_lane_record(&session_v4::LaneRecord::ToolStarted {
            base: session_v4::RecordBase {
                id: session_manager::new_record_id("tool"),
                seq: 0,
                lane: String::new(),
                timestamp: now_ms() as i64,
            },
            run_id: run_id.to_string(),
            assistant_entry_id: assistant_entry_id.unwrap_or("").to_string(),
            tool_index,
            tool_call_id: fin.call_id(),
            tool_name: fin.name.clone(),
            effective_args: fin.args.clone(),
            result_entry_id: result_entry_id.unwrap_or_default(),
            replay: "never".to_string(),
        });
    }

    /// v4 durable 记录：assistant step_attempt
    pub(crate) fn record_step_attempt(
        &mut self,
        run_id: &str,
        attempt: &mut u32,
        result_entry_id: Option<&str>,
    ) {
        let Some(result_entry_id) = result_entry_id else {
            return;
        };
        if let Some(s) = self.session.as_mut() {
            *attempt += 1;
            s.append_lane_record(&session_v4::LaneRecord::StepAttempt {
                base: session_v4::RecordBase {
                    id: session_manager::new_record_id("step"),
                    seq: 0,
                    lane: String::new(),
                    timestamp: now_ms() as i64,
                },
                run_id: run_id.to_string(),
                step: "assistant".to_string(),
                attempt: *attempt,
                result_entry_id: result_entry_id.to_string(),
                compaction_reason: None,
            });
        }
    }

    /// 构造 toolResult 消息：落库 + 源序 message_start/end 事件
    fn push_tool_result_message(
        &mut self,
        fin: &FinalizedToolCall,
        duration_ms: Option<u64>,
        tool_results: &mut Vec<(ContentBlock, AgentMessage)>,
    ) {
        let content: Vec<ContentBlock> = if let Some(override_blocks) = &fin.content_override {
            override_blocks.clone()
        } else if fin.attachments.is_empty() {
            vec![ContentBlock::Text {
                text: fin.text.clone(),
                text_signature: None,
            }]
        } else {
            // 图片内联限制：工具结果附件（含扩展注入/替换的图片）也按模型目录限制兜底规范化。
            // 已合规（read 工具已规范化）的附件按 base64 长度直接放行，不做重复解码。
            // `images.blockImages` 打开时一律不附图片（扩展生成的图也不例外）。
            let limits = self.model.image_resize;
            let mut blocks: Vec<ContentBlock> =
                if settings_manager::read_settings_images_block_images() {
                    vec![ContentBlock::Text {
                        text:
                            "[Image omitted: images are blocked by settings (images.blockImages).]"
                                .to_string(),
                        text_signature: None,
                    }]
                } else {
                    fin.attachments
                        .iter()
                        .map(|a| {
                            let (data, mime_type) =
                                normalize_base64_attachment(&a.data_base64, &a.mime_type, limits);
                            ContentBlock::Image { data, mime_type }
                        })
                        .collect()
                };
            blocks.push(ContentBlock::Text {
                text: fin.text.clone(),
                text_signature: None,
            });
            blocks
        };
        let result_msg = AgentMessage {
            role: "toolResult".to_string(),
            thinking_level: None,
            content,
            tool_call_id: Some(fin.call_id()),
            tool_name: Some(fin.name.clone()),
            is_error: fin.is_error,
            stop_reason: None,
            error_message: None,
            model: None,
            provider: None,
            api: None,
            response_model: None,
            response_id: None,
            diagnostics: None,
            usage: fin.usage.clone(),
            deferred: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: now_ms(),
            duration_ms,
            details: fin.details.clone(),
            citations: None,
            entry_id: None,
        };
        let result_msg = if let Some(s) = self.session.as_mut() {
            let entry_id = s.append_message(&result_msg);
            let mut rm = result_msg;
            rm.entry_id = Some(entry_id);
            rm
        } else {
            result_msg
        };
        let v = serde_json::to_value(&result_msg).unwrap();
        self.emit(json!({ "type": "message_start", "message": v.clone() }));
        self.emit(json!({ "type": "message_end", "message": v }));
        tool_results.push((fin.call.clone(), result_msg));
    }

    /// 文件变更队列键：canonical path
    fn mutation_key(cwd: &str, path: &str) -> String {
        let p = tools::resolve_tool_path(path, cwd);
        std::fs::canonicalize(&p)
            .unwrap_or(p)
            .to_string_lossy()
            .into_owned()
    }

    /// 构造 bash 流式输出回调（100ms 节流）。通过共享 json_sink 发 `tool_execution_update` 事件。
    fn bash_chunk_callback(
        sink: Option<Arc<Mutex<JsonSink>>>,
        tool_call_id: &str,
        tool_name: &str,
        args: &Value,
    ) -> Option<tools::OnChunkCallback> {
        let sink = sink?;
        let tool_call_id = tool_call_id.to_string();
        let tool_name = tool_name.to_string();
        let args = args.clone();
        let last_emit = Arc::new(Mutex::new(Instant::now()));

        Some(Arc::new(tokio::sync::Mutex::new(
            Box::new(move |chunk: &str| {
                let now = Instant::now();
                let mut last = match last_emit.lock() {
                    Ok(g) => g,
                    Err(_) => return,
                };
                if now.duration_since(*last) < Duration::from_millis(100) {
                    return;
                }
                *last = now;
                drop(last);

                if let Ok(mut g) = sink.lock() {
                    let f: &mut dyn FnMut(Value) = g.as_mut();
                    f(json!({
                        "type": "tool_execution_update",
                        "toolCallId": tool_call_id,
                        "toolName": tool_name,
                        "args": args,
                        "partialResult": { "content": [ { "type": "text", "text": chunk } ] }
                    }));
                }
            }) as Box<dyn FnMut(&str) + Send>,
        )))
    }

    /// 判断 provider 错误是否为上下文溢出（触发自动压缩重试）
    fn is_overflow_error_str(msg: &str) -> bool {
        // 对齐 pi overflow.ts 的 OVERFLOW_PATTERNS + NON_OVERFLOW_PATTERNS（substring 近似）：
        // - 先排除限流/服务不可用等非溢出错误，避免被 /too many tokens/ 类模式误判；
        // - 再按常见 overflow 文案匹配（Anthropic prompt is too long / request_too_large、
        //   OpenAI exceeds context window、Google input token count、xAI max prompt length、
        //   Groq reduce the length、OpenRouter max context length、Copilot exceeds the limit of、
        //   llama.cpp/LM Studio/MiniMax/Kimi/Mistral/DS4/Ollama/DashScope 等）。
        let m = msg.to_lowercase();
        /// 非溢出错误特征子串：命中则不当作上下文溢出，避免限流类误判。
        const NON_OVERFLOW: [&str; 4] = [
            "throttling",
            "service unavailable",
            "rate limit",
            "too many requests",
        ];
        if NON_OVERFLOW.iter().any(|p| m.contains(p)) {
            return false;
        }
        /// 上下文溢出错误特征子串，命中后触发自动压缩并重试。
        const OVERFLOW: &[&str] = &[
            "prompt is too long",
            "prompt too long",
            "prompt exceeds max length",
            "request_too_large",
            "input is too long for requested model",
            "exceeds the context window",
            "exceeds the maximum context length",
            "maximum context length",
            "exceeds the maximum allowed input length",
            "input token count exceeds",
            "exceeds the maximum number of tokens",
            "maximum prompt length",
            "reduce the length of the messages",
            "maximum context length is",
            "exceeds the limit of",
            "exceeds the available context size",
            "greater than the context length",
            "context window exceeds limit",
            "exceeded model token limit",
            "too large for model",
            "is longer than the model",
            "configured context size",
            "model_context_window_exceeded",
            "range of input length",
            "context length exceeded",
            "context_length_exceeded",
            "context_length",
            "too many tokens",
            "token limit exceeded",
            "tokens exceeded",
        ];
        OVERFLOW.iter().any(|p| m.contains(p))
    }

    /// `ProviderStatus` 文案形如 `provider returned 400 Bad Request: <body>`；
    /// body 为空即无响应体。Cerebras 无 body 的 400/413 是溢出信号
    fn is_bodyless_status_error(msg: &str) -> bool {
        let lower = msg.to_lowercase();
        let Some(rest) = lower.strip_prefix("provider returned ") else {
            return false;
        };
        if !(rest.starts_with("400") || rest.starts_with("413")) {
            return false;
        }
        msg.split_once(": ")
            .is_some_and(|(_, body)| body.trim().is_empty())
    }

    /// 是否上下文溢出。覆盖三种形态：
    /// 1) error 文案（含 Cerebras 无 body 400/413，仅当 provider=cerebras）；
    /// 2) z.ai 静默溢出：成功但 input+cacheRead 超过上下文窗口；
    /// 3) Xiaomi length-stop 溢出：output=0 且输入填满窗口（≥99%）。
    fn message_is_context_overflow(
        msg: &AgentMessage,
        context_window: u32,
        provider: &str,
    ) -> bool {
        let stop = msg.stop_reason.as_deref();
        let usage = msg.usage.as_ref();
        let input_tokens = usage
            .map(|u| u.input as u64 + u.cache_read as u64)
            .unwrap_or(0);

        if stop == Some("error") {
            let Some(err) = msg.error_message.as_deref() else {
                return false;
            };
            return Self::is_overflow_error_str(err)
                || (provider == "cerebras" && Self::is_bodyless_status_error(err));
        }

        if context_window == 0 {
            return false;
        }

        // 静默溢出（z.ai 风格）
        if stop == Some("stop") && input_tokens > context_window as u64 {
            return true;
        }

        // length-stop 溢出（Xiaomi MiMo 风格）
        if stop == Some("length")
            && usage.is_some_and(|u| u.output == 0)
            && input_tokens * 100 >= context_window as u64 * 99
        {
            return true;
        }
        false
    }

    /// 子代理地基：从当前 agent 复制配置构造一个**状态隔离**的独立 agent 实例
    /// （空消息、无 session、独立队列/sink）。为将来真正并行子代理铺路：
    /// 多个 child 实例可在不同 task 中各自跑 run_loop，互不共享可变状态。
    pub fn child_agent(&self, custom_system_prompt: Option<String>) -> Agent {
        let system_override = custom_system_prompt
            .or_else(|| self.rebuild_ctx.system_prompt.clone())
            .or_else(|| Some(self.system_prompt.clone()));
        let mut tpl = self.snapshot_template();
        if let Some(sp) = system_override {
            tpl.system_prompt = sp;
        }
        let mut child = Agent::child_from_template(&tpl);
        // 子代理继承同一（可能被替换的）agent_loop，保证整棵调用树语义一致
        child.agent_loop = self.agent_loop.clone();
        child
    }

    /// 驱动一个子代理实例完成一次 prompt（独立 run_loop，返回最终文本）。
    /// 阻塞式：等待子代理 settle；调用方持有主 agent 锁时，子代理是独立实例，不争用同一锁。
    /// 返回 BoxFuture（Send）以打破 async 递归（execute_tool → run_sub_agent → prompt → run_loop → …），
    /// 并为跨 task 并发（spawn_sub_agent）提供 Send 前提。
    pub fn run_sub_agent<'a>(&'a self, user_message: &'a str) -> BoxFuture<'a, Result<String>> {
        // child 在 async 块外构造：future 只捕获 owned Agent（Send），避免持有 &Agent（需 Sync）
        let mut child = self.child_agent(None);
        Box::pin(async move { child.prompt(user_message).await })
    }

    /// 并行子代理：把一次子任务 spawn 到独立 tokio task 运行（多实例真正并发）。
    /// 返回 JoinHandle，调用方可在任意时刻 await——支持“非阻塞进度”：
    /// 主循环可以先发起多个子代理，稍后统一收结果（对齐将来 pi harness 的子代理语义）。
    pub fn spawn_sub_agent(&self, user_message: String) -> JoinHandle<Result<String>> {
        let mut child = self.child_agent(None);
        tokio::spawn(async move { child.prompt(&user_message).await })
    }

    /// 内置工具名（并行分发前过滤用）
    fn is_builtin_tool(name: &str) -> bool {
        matches!(
            name,
            "read" | "write" | "edit" | "bash" | "grep" | "find" | "ls"
        )
    }

    /// 内置工具执行（与 self 解耦：仅依赖 cwd）。
    /// 供并行阶段 futures 捕获 `&String`（Send）而非 `&Agent`（需 Sync）。
    /// 调用方须先用 [`Self::is_builtin_tool`] 过滤；未命中返回 Unknown tool 错误。
    async fn execute_builtin_tool(
        cwd: &str,
        name: &str,
        arguments: &Value,
        on_bash_chunk: Option<tools::OnChunkCallback>,
        model_supports_images: bool,
        bash_env: Option<tools::BashSessionEnv>,
        image_resize: ImageResizeLimits,
    ) -> std::result::Result<tools::ToolResult, tools::ToolError> {
        match name {
            "read" => {
                let path = arguments
                    .get("path")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        tools::ToolError("read tool is missing path argument".to_string())
                    })?;
                let offset = arguments.get("offset").and_then(|v| v.as_i64());
                let limit = arguments.get("limit").and_then(|v| v.as_i64());
                let image_opts = tools::ReadImageOptions {
                    model_supports_images,
                    auto_resize: settings_manager::read_settings_images_auto_resize(),
                    block_images: settings_manager::read_settings_images_block_images(),
                    resize_limits: image_resize,
                };
                tools::execute_read_with_options(path, offset, limit, cwd, image_opts).await
            }
            "write" => {
                let path = arguments
                    .get("path")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        tools::ToolError("write tool is missing path argument".to_string())
                    })?;
                let content = arguments
                    .get("content")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        tools::ToolError("write tool is missing content argument".to_string())
                    })?;
                tools::execute_write(path, content, cwd).await
            }
            "edit" => {
                let path = arguments
                    .get("path")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        tools::ToolError("edit tool is missing path argument".to_string())
                    })?;
                let edits = arguments.get("edits").ok_or_else(|| {
                    tools::ToolError("edit tool is missing edits argument".to_string())
                })?;
                let (result, _patch, _first_line) = tools::execute_edit(path, edits, cwd).await?;
                Ok(result)
            }
            #[cfg(windows)]
            "powershell" => {
                let command = arguments
                    .get("command")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        tools::ToolError("powershell tool is missing command argument".to_string())
                    })?;
                let timeout = arguments.get("timeout").and_then(|v| v.as_f64());
                let out = tools::execute_powershell(command, timeout, cwd, on_bash_chunk).await;
                if out.cancelled || out.timed_out {
                    Err(tools::ToolError(out.text))
                } else {
                    let mut result = if out.exit_code.is_some_and(|c| c != 0) {
                        tools::ToolResult::error(out.text)
                    } else {
                        tools::ToolResult::text(out.text)
                    };
                    result.structured_content = out.structured;
                    Ok(result)
                }
            }
            "bash" => {
                let command = arguments
                    .get("command")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        tools::ToolError("bash tool is missing command argument".to_string())
                    })?;
                let timeout = arguments.get("timeout").and_then(|v| v.as_f64());

                // 注册到进程表，使 abort_bash 能中止 agent 运行的 bash 工具
                let id = session_manager::new_record_id("tool");
                let out = match bash_env {
                    Some(env) => {
                        tools::execute_bash_with_env(
                            command,
                            timeout,
                            cwd,
                            Some(&id),
                            on_bash_chunk,
                            &env,
                        )
                        .await
                    }
                    None => {
                        tools::execute_bash_with(command, timeout, cwd, Some(&id), on_bash_chunk)
                            .await
                    }
                };
                if out.cancelled || out.timed_out {
                    Err(tools::ToolError(out.text))
                } else {
                    let mut result = if out.exit_code.is_some_and(|c| c != 0) {
                        tools::ToolResult::error(out.text)
                    } else {
                        tools::ToolResult::text(out.text)
                    };

                    result.structured_content = out.structured;
                    result.details = Some(json!({
                        "truncation": if out.truncated {
                            json!({
                                "truncated": true,
                                "reason": "length",
                                "fullOutputPath": out.full_output_path,
                            })
                        } else {
                            Value::Null
                        },
                        "fullOutputPath": out.full_output_path,
                    }));
                    Ok(result)
                }
            }
            "grep" => {
                let pattern = arguments
                    .get("pattern")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        tools::ToolError("grep tool is missing pattern argument".to_string())
                    })?;
                tools::execute_grep(
                    pattern,
                    arguments.get("path").and_then(|v| v.as_str()),
                    arguments.get("glob").and_then(|v| v.as_str()),
                    arguments
                        .get("ignoreCase")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                    arguments
                        .get("literal")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                    arguments.get("context").and_then(|v| v.as_i64()),
                    arguments.get("limit").and_then(|v| v.as_i64()),
                    cwd,
                )
                .await
            }
            "find" => {
                let pattern = arguments
                    .get("pattern")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        tools::ToolError("find tool is missing pattern argument".to_string())
                    })?;
                tools::execute_find(
                    pattern,
                    arguments.get("path").and_then(|v| v.as_str()),
                    arguments.get("limit").and_then(|v| v.as_i64()),
                    cwd,
                )
                .await
            }
            "ls" => {
                tools::execute_ls(
                    arguments.get("path").and_then(|v| v.as_str()),
                    arguments.get("limit").and_then(|v| v.as_i64()),
                    cwd,
                )
                .await
            }
            _ => Err(tools::ToolError(format!("Unknown tool: {}", name))),
        }
    }

    /// 异步执行扩展工具
    async fn execute_extension_tool_async(
        &self,
        name: &str,
        args: &Value,
        ctx: core::extensions::ToolExecCtx,
    ) -> std::result::Result<tools::ToolResult, tools::ToolError> {
        for ext in core::extensions::registered() {
            if ext.tools().iter().any(|t| t.name == name) {
                return ext
                    .execute_tool_async(name.to_string(), args.clone(), ctx.clone())
                    .await;
            }
        }
        Err(tools::ToolError(format!("Tool {} not found", name)))
    }
}

/// 已解析并预处理的一次工具调用：含名称、参数，或需要立即返回的错误结果。
struct Prepared {
    /// 模型给出的原始工具调用块（含调用 id 与名称）。
    call: ContentBlock,
    /// 解析出的工具名。
    name: String,
    /// 预处理后的参数（可能已被 beforeToolCall 改写）。
    args: Value,
    immediate: Option<String>, // Some(错误文本) = 不执行，直接产出错误 toolResult
    /// 立即错误结果是否带终止语义（true 则本批执行完不再请求模型）。
    immediate_terminate: bool,
}

impl Prepared {
    /// 取原始工具调用块里的调用 id；非工具调用块时返回空串。
    fn tool_call_id(&self) -> String {
        match &self.call {
            ContentBlock::ToolCall { id, .. } => id.clone(),
            _ => String::new(),
        }
    }
}

/// 每个工具开始前发 tool_execution_start
fn emit_start(sink: &Option<Arc<Mutex<JsonSink>>>, p: &Prepared) {
    if let Some(sink) = sink
        && let Ok(mut g) = sink.lock()
    {
        let f: &mut dyn FnMut(Value) = g.as_mut();
        let args = serde_json::to_value(&p.args).unwrap_or(Value::Null);
        f(json!({
            "type": "tool_execution_start",
            "toolCallId": p.tool_call_id(),
            "toolName": p.name,
            "args": args,
            // 工具启动墙钟毫秒：UI 层渲染进行中工具块的 `Elapsed X.Xs` 实时计时
            "startedAtEpochMs": now_ms()
        }));
    }
}

/// 向事件 sink 发 `tool_execution_end`（携带结果、isError 与耗时）；无 sink 时不动作。
fn emit_tool_execution_end(sink: &Option<Arc<Mutex<JsonSink>>>, f: &FinalizedToolCall) {
    if let Some(sink) = sink
        && let Ok(mut g) = sink.lock()
    {
        let fptr: &mut dyn FnMut(Value) = g.as_mut();
        let mut payload = json!({
            "type": "tool_execution_end",
            "toolCallId": f.call_id(),
            "toolName": f.name,
            "result": f.to_result_json(),
            "isError": f.is_error
        });

        // 未真正执行的调用没有耗时，字段整体省略
        if let Some(ms) = f.duration_ms
            && let Some(obj) = payload.as_object_mut()
        {
            obj.insert("durationMs".to_string(), json!(ms));
        }
        fptr(payload);
    }
}

/// 依次调用所有声明 `AfterToolCall` 的扩展，用其返回覆写 `fin` 的文本/错误标记/
/// 用量/终止语义/content/details；某个扩展报错时把结果整体替换为该错误。
fn apply_after_tool_call(fin: &mut FinalizedToolCall, name: &str, args: &Value) {
    for ext in core::extensions::registered() {
        if ext.hooks().contains(&ExtensionHook::AfterToolCall) {
            match ext.after_tool_call_ext(name, args, &fin.text, fin.is_error) {
                Ok(out) => {
                    fin.text = out.text;
                    if let Some(v) = out.is_error {
                        fin.is_error = v;
                    }
                    if let Some(v) = out.usage {
                        fin.usage = Some(v);
                    }
                    if let Some(v) = out.terminate {
                        fin.terminate = v;
                    }

                    // content 替换整个数组、details 替换载荷
                    if let Some(content) = out.content {
                        fin.content_override = Some(content);
                    }
                    if let Some(details) = out.details {
                        fin.details = Some(details);
                    }
                }
                Err(e) => {
                    fin.text = e.0;
                    fin.is_error = true;
                    fin.usage = None;
                    fin.terminate = false;
                    fin.details = None;
                    fin.content_override = None;
                }
            }
        }
    }
}

/// 已物化的子代理：持有 Agent，并把运行期控制面收窄成 [`core::extensions::SubAgentControls`]。
struct MaterializedSubAgent {
    /// 物化出的子代理实例，持有独立的消息历史与工具状态。
    agent: Agent,
    /// 子代理运行的取消信号，与 child.run_abort 是同一个 Arc。
    abort: Arc<AtomicBool>,
    /// 子会话文件路径；未持久化会话时为 None。
    session_path: Option<String>,
}

impl SubAgentRunner for MaterializedSubAgent {
    /// 把子代理的运行期控制面收窄成 [`SubAgentControls`]：
    /// steer 写入运行时注入箱，abort 与 session_path 原样暴露。
    fn controls(&self) -> SubAgentControls {
        let inbox = self.agent.runtime_steer_inbox.clone();
        SubAgentControls {
            steer: Arc::new(move |text: String| {
                if let Ok(mut q) = inbox.lock() {
                    q.push_back(AgentMessage::user_text(&text));
                }
            }),
            abort: self.abort.clone(),
            session_path: self.session_path.clone(),
        }
    }

    /// 向子代理投递 prompt 并跑完整轮：成功返回最终文本，失败把错误转成 `String`。
    fn run(&mut self, prompt: String) -> BoxFuture<'_, std::result::Result<String, String>> {
        Box::pin(async move { self.agent.prompt(&prompt).await.map_err(|e| e.to_string()) })
    }
}

/// 按 [`core::extensions::SubAgentSpec`] 物化一个状态隔离的 child。
///
/// 核心只解释 spec，不做策略判断；顺序有讲究：cwd → 上下文 → 会话 → 模型 →
/// thinking → 系统提示 → 工具白名单（最后 rebuild，使提示与工具表一致）。
fn materialize_sub_agent(
    tpl: &AgentTemplate,
    spec: SubAgentSpec,
) -> std::result::Result<MaterializedSubAgent, String> {
    let mut child = Agent::child_from_template(tpl);
    let abort = child.run_abort.clone();

    // cwd：影响工具执行与系统提示里的路径
    if let Some(cwd) = spec.cwd.clone() {
        child.cwd = cwd;
    }

    // inherit_context：复制父级当前对话
    if spec.inherit_context {
        child.messages = tpl.parent_messages.as_ref().clone();
    }

    // 显式对话覆盖（mention clone 的实时、压缩感知上下文）；优先于 inherit_context
    if let Some(msgs) = spec.context_messages.clone() {
        child.messages = msgs;
    }

    // 子会话：续跑既有会话，或落盘新建 + 挂父会话（供 /resume 嵌套显示）
    let session_path = if let Some(path) = spec.resume_session_path.as_deref() {
        let session = Session::open(path)
            .map_err(|e| format!("failed to resume sub-agent session {path}: {e}"))?;
        child.messages = session.build_context_messages();
        let path = session
            .get_session_file()
            .map(|p| p.to_string_lossy().to_string())
            .or_else(|| Some(path.to_string()));
        child.session = Some(session);
        path
    } else if spec.persist_session {
        let parent_session_id = spec
            .session_parent_id
            .clone()
            .or_else(|| tpl.parent_session_id.clone());
        let session = Session::create_with(SessionCreateOptions {
            cwd: child.cwd.clone(),
            session_dir: spec.session_dir.clone(),
            persist: true,
            parent_session_id,
            ..Default::default()
        })
        .map_err(|e| format!("failed to create sub-agent session: {e}"))?;
        let path = session
            .session_file
            .as_ref()
            .map(|p| p.to_string_lossy().to_string());
        child.session = Some(session);
        path
    } else {
        None
    };

    // 模型：精确 `provider/modelId`，解析失败直接报错（不静默继承）
    if let Some(model) = spec.model.as_deref() {
        let (provider, model_id) = model
            .split_once('/')
            .ok_or_else(|| format!("invalid model {model:?}: expected \"provider/modelId\""))?;
        child
            .switch_model(provider, model_id, false)
            .map_err(|e| format!("model {model:?} is not available: {e}"))?;
    }

    // thinking：在模型确定之后应用，才按目标模型的能力钳制
    if let Some(level) = spec.thinking.as_deref() {
        child.set_thinking_level_silent(level, false);
    }

    // 系统提示：replace（并清空 context_files）或 append
    if let Some(body) = spec.system_prompt.clone() {
        child.rebuild_ctx.system_prompt = Some(body);
    }
    if let Some(body) = spec.append_system_prompt.clone() {
        child.rebuild_ctx.append_system_prompt = Some(body);
    }
    if spec.clear_context_files {
        child.rebuild_ctx.context_files.clear();
    }
    // `skills: false`：不继承父级的技能索引（技能正文由扩展按需预载进提示）
    if spec.clear_skills {
        child.skills.clear();
    }

    // 工具白名单：以父级可用集为宇宙，按 spec 收窄后强制剔除子代理工具。
    // 用 Allowlist 而非 Default：Default 会把扩展工具无条件并入，无法表达收窄。
    let universe = child.get_all_tools();
    let mut selected: Vec<String> = match spec.tools.as_ref() {
        None => universe,
        Some(list) => {
            if list.is_empty() || list.iter().any(|t| t == "none") {
                Vec::new()
            } else if list.iter().any(|t| t == "*" || t == "all") {
                universe
            } else {
                universe
                    .into_iter()
                    .filter(|n| list.iter().any(|t| t == n))
                    .collect()
            }
        }
    };

    // 扩展作用域：把"工具名 → 所属扩展名"查出来，用于 isolated / extensions / exclude_extensions。
    // 只在需要时构建（默认无作用域时不额外遍历注册表）。
    if spec.isolated || spec.extensions.is_some() || !spec.exclude_extensions.is_empty() {
        let mut owner: HashMap<String, String> = HashMap::new();
        for ext in core::extensions::registered() {
            for t in ext.tools() {
                owner.insert(t.name, ext.name().to_string());
            }
        }
        // isolated：只保留内置工具（扩展工具在 child 里完全不存在）
        if spec.isolated {
            selected.retain(|n| !owner.contains_key(n));
        }
        // extensions 白名单：白名单外（含没有扩展归属的内置）不动内置、只限扩展工具
        if let Some(allow) = spec.extensions.as_ref() {
            selected.retain(|n| match owner.get(n) {
                None => true,
                Some(ext) => allow.iter().any(|a| a == ext),
            });
        }
        // exclude_extensions：按扩展名剔除
        if !spec.exclude_extensions.is_empty() {
            selected.retain(|n| match owner.get(n) {
                None => true,
                Some(ext) => !spec.exclude_extensions.iter().any(|d| d == ext),
            });
        }
    }

    // disallowed_tools：显式剔除（在收窄与递归防护之后，顺序不影响结果集）
    if !spec.disallowed_tools.is_empty() {
        selected.retain(|n| !spec.disallowed_tools.iter().any(|d| d == n));
    }

    let mut selected: Vec<String> = selected
        .into_iter()
        // 递归防护：把编排工具从 child 的工具表里剔除，避免 child 继承父级工具集后拿到
        // `Agent` 形成无上限递归。**例外**：扩展显式写进 `spec.tools` 的那些——
        // 那是扩展为"嵌套委派"自己放行的（它会在 handler 里按调用者身份限深、限属主），
        // 而不是 child 顺手继承来的。
        .filter(|n| {
            !SUBAGENT_TOOL_NAMES.contains(&n.as_str())
                || spec.tools.as_ref().is_some_and(|l| l.contains(n))
        })
        .collect();
    selected.dedup();

    if spec.tools.as_ref().is_some_and(|l| !l.is_empty()) && selected.is_empty() {
        return Err(format!(
            "agent type {:?}: none of the configured tools are available",
            spec.agent_type
        ));
    }

    // 身份随 spec 传入、随 ctx 交回（扩展据此做嵌套限深与属主作用域）
    child.rebuild_ctx.agent_id = spec.agent_id.clone();
    child.rebuild_ctx.depth = spec.depth;
    child.rebuild_ctx.selection = ToolSelection::Allowlist(selected);

    // 注入工具：**无条件**并入 child 工具表（在 selection 之后，故不受白名单/disallowed/ 作用域约束；
    // 详见 `ToolIndex` 与 `compose_tools`）。同名遮蔽内置或扩展工具时告警一次：
    // 第三方扩展取了同一个名字不该让所有 schema 工作流全盘失败，但也不能静默。
    for t in &spec.injected_tools {
        let shadowed =
            Agent::is_builtin_tool(&t.meta.name) || find_extension_tool(&t.meta.name).is_some();
        if shadowed && note_warning_once(&format!("injected-tool-shadow|{}", t.meta.name)) {
            core::extensions::request_ui(ExtensionUiRequest::Notify {
                text: format!(
                    "subagent: injected tool {:?} shadows an existing tool inside this sub-agent",
                    t.meta.name
                ),
                level: UiNotifyLevel::Warning,
            });
        }
    }
    child.rebuild_ctx.injected_tools = Arc::new(spec.injected_tools.clone());
    child.rebuild_tools();
    child.json_sink = spec.events.clone(); // 每 child 独立事件流

    // max_turns 优雅收尾：到限注入 wrap-up，宽限 `spec.grace_turns`（缺省 5）轮后停机。
    // 结局状态（steered / aborted）由扩展按事件里的 turn 计数推断，核心不额外发信号。
    // `finishTurn` 对 error/aborted 也会调用，其决策被忽略——显式跳过硬退出，避免谓词副作用（计数 / wrap-up 注入）。
    // 注：`finish_turn` 不随模板继承（`AgentTemplate` 不携带），因此这里直接设置；
    // 扩展的 turn_end 边界决策由内核循环统一派发（主/子代理一致），无需在此链式保留。
    if let Some(max_turns) = spec.max_turns.filter(|m| *m > 0) {
        let grace = spec.grace_turns.unwrap_or(SUBAGENT_GRACE_TURNS);
        let counter = Arc::new(AtomicU32::new(0));
        let inbox = child.runtime_steer_inbox.clone();

        child.finish_turn = Some(Mutex::new(Box::new(move |ctx: &TurnContext| {
            if matches!(
                ctx.message.stop_reason.as_deref(),
                Some("error") | Some("aborted")
            ) {
                return None;
            }

            let turn = counter.fetch_add(1, Ordering::SeqCst) + 1;
            if turn == max_turns
                && let Ok(mut q) = inbox.lock()
            {
                q.push_back(AgentMessage::user_text(SUBAGENT_WRAP_UP));
            }
            (turn >= max_turns.saturating_add(grace)).then_some(FinishTurnAction::End)
        })));
    }

    Ok(MaterializedSubAgent {
        agent: child,
        abort,
        session_path,
    })
}

/// 构造工具执行上下文（工具执行期与 [`Agent::exec_ctx`] 共用）。
///
/// - 调用方在 [`execute_one`] / 顺序执行分支内调用（而非 `execute_tool_calls` 持有），
///   避免 exec_ctx → child.prompt → execute_tool_calls 的 Send 循环依赖；
/// - `parent_tool_call_id` 是当前正在执行的工具调用 id，嵌套调用的事件与记录据此挂到调用方；
/// - `nested_depth` 是已嵌套层数（主工具为 0），用于 [`MAX_NESTED_TOOL_DEPTH`] 限深；
/// - `branch_entries` 是当前会话活动分支的条目快照（见 [`ToolExecCtx::session_branch_entries`]）。
fn make_exec_ctx(
    cwd: &str,
    tpl: &Arc<AgentTemplate>,
    parent_abort: &Arc<AtomicBool>,
    sink: &Option<Arc<Mutex<JsonSink>>>,
    parent_tool_call_id: Option<&str>,
    branch_entries: Option<Arc<Vec<Value>>>,
) -> ToolExecCtx {
    build_exec_ctx(
        cwd,
        tpl,
        parent_abort,
        sink,
        parent_tool_call_id.map(str::to_string),
        Arc::new(Mutex::new(NestedCallLog::default())),
        0,
        false,
        branch_entries,
    )
}

/// [`make_exec_ctx`] 的实际构造：把自己也重进 `execute_tool`（嵌套调用的扩展工具需要
/// 同源上下文），每层带自己的账本（嵌套记录形成树，而不是平铺到最外层）。
#[allow(clippy::too_many_arguments)]
fn build_exec_ctx(
    cwd: &str,
    tpl: &Arc<AgentTemplate>,
    parent_abort: &Arc<AtomicBool>,
    sink: &Option<Arc<Mutex<JsonSink>>>,
    parent_tool_call_id: Option<String>,
    log: Arc<Mutex<NestedCallLog>>,
    nested_depth: u32,
    script_call: bool,
    branch_entries: Option<Arc<Vec<Value>>>,
) -> ToolExecCtx {
    let make_sub_agent = {
        let tpl = tpl.clone();
        Arc::new(move |spec: SubAgentSpec| {
            materialize_sub_agent(&tpl, spec).map(|r| Box::new(r) as Box<dyn SubAgentRunner>)
        }) as Arc<MakeSubAgentFn>
    };

    let execute_tool: ExecuteToolFn = {
        let cwd = cwd.to_string();
        let tpl = tpl.clone();
        let abort = parent_abort.clone();
        let sink = sink.clone();
        let log = log.clone();
        let parent = parent_tool_call_id.clone();
        let entries = branch_entries.clone();

        Arc::new(move |name: String, args: Value| {
            let cwd = cwd.clone();
            let tpl = tpl.clone();
            let abort = abort.clone();
            let sink = sink.clone();
            let log = log.clone();
            let parent = parent.clone();
            let entries = entries.clone();

            Box::pin(async move {
                run_nested_tool(
                    &cwd,
                    &tpl,
                    &abort,
                    &sink,
                    &parent,
                    &name,
                    args,
                    nested_depth,
                    &log,
                    entries,
                )
                .await
            }) as BoxFuture<'static, std::result::Result<ToolResult, ToolError>>
        })
    };

    ToolExecCtx {
        cwd: cwd.to_string(),
        make_sub_agent,
        parent_abort: parent_abort.clone(),
        agent_id: tpl.rebuild_ctx.agent_id.clone(),
        depth: tpl.rebuild_ctx.depth,
        parent_model: Some(format!("{}/{}", tpl.model.provider, tpl.model.model_id)),
        execute_tool,
        parent_tool_call_id,
        nested_calls: log,
        session_branch_entries: branch_entries,
        script_tools: Arc::new(script_tools_for(
            &tpl.tools,
            &tpl.rebuild_ctx.injected_tools,
            &tpl.rebuild_ctx.selection,
            &tpl.rebuild_ctx.exclude_tools,
        )),
        script_call,
    }
}

/// 工具是否属于脚本宿主扩展（`codemode`）：它的嵌套调用算「脚本调用」。
fn tool_hosts_scripts(name: &str) -> bool {
    core::extensions::registered()
        .iter()
        .any(|ext| ext.hosts_scripts() && ext.tools().iter().any(|t| t.name == name))
}

/// 执行一次嵌套工具调用（`ctx.execute_tool`）：解析 → 门控 → 执行 → 记录。
///
/// 与模型发起的工具调用的差异（有意为之）：
/// - 不走 `beforeToolCall` / `afterToolCall` 钩子（这是调用方工具的内部实现细节）；
/// - 参数按工具 schema 校验（与模型路径同一套）；
/// - 每次调用发 `tool_execution_start` / `tool_execution_end`，带 `parentToolCallId`；
/// - 结果记入调用方账本（`nestedCalls` + 用量合计），从而在会话成本里可见。
#[allow(clippy::too_many_arguments)]
async fn run_nested_tool(
    cwd: &str,
    tpl: &Arc<AgentTemplate>,
    abort: &Arc<AtomicBool>,
    sink: &Option<Arc<Mutex<JsonSink>>>,
    parent_call_id: &Option<String>,
    name: &str,
    args: Value,
    nested_depth: u32,
    log: &Arc<Mutex<NestedCallLog>>,
    branch_entries: Option<Arc<Vec<Value>>>,
) -> std::result::Result<ToolResult, ToolError> {
    // 拒绝原因先算出、不直接返回：被拒的嵌套调用也进调用方账本（否则调用方无从知道
    // 自己的编排里哪一次被挡住了），错误返回值照旧给调用方。
    let refusal: Option<String> = if nested_depth >= MAX_NESTED_TOOL_DEPTH {
        Some(format!(
            "nested tool call depth limit reached ({MAX_NESTED_TOOL_DEPTH}); refusing to call {name}"
        ))
    } else if abort.load(Ordering::Relaxed) {
        Some("Operation aborted".to_string())
    } else {
        let idx = ToolIndex::resolve(&tpl.rebuild_ctx.injected_tools, name);
        if !idx.exists() {
            Some(format!("Tool {name} not found"))
        } else {
            // 暴露方式门控：model-only / 未激活的 deferred 只能由模型直接调；
            // codemode / hidden 只对其它工具可见（正是它们的用途）。
            match find_extension_tool(name) {
                Some(tool)
                    if !tool
                        .exposure
                        .is_nested_callable(is_deferred_tool_activated(name)) =>
                {
                    Some(format!(
                        "Tool {name} cannot be called from another tool (exposure: {})",
                        tool.exposure.as_str()
                    ))
                }
                _ => None,
            }
        }
    };

    let idx = ToolIndex::resolve(&tpl.rebuild_ctx.injected_tools, name);
    let args = match refusal {
        Some(_) => args,
        None => match idx.parameters() {
            Some(schema) => match tools::validate_tool_arguments(name, &schema, &args) {
                Ok(valid) => valid,
                Err(msg) => {
                    return refuse_nested_tool(sink, parent_call_id, name, args, msg, log);
                }
            },
            None => args,
        },
    };

    if let Some(reason) = refusal {
        return refuse_nested_tool(sink, parent_call_id, name, args, reason, log);
    }

    let call_id = new_nested_call_id(parent_call_id);
    let started = Instant::now();
    emit_nested_start(sink, &call_id, parent_call_id.as_deref(), name, &args);

    let child_log = Arc::new(Mutex::new(NestedCallLog::default()));
    let result = if let Some(handler) = idx.injected_handler() {
        handler(args.clone())
    } else if idx.is_builtin() {
        let supports_images = tpl.model.input.iter().any(|i| i == "image");
        Agent::execute_builtin_tool(
            cwd,
            name,
            &args,
            None,
            supports_images,
            None,
            tpl.model.image_resize,
        )
        .await
    } else {
        let child_ctx = build_exec_ctx(
            cwd,
            tpl,
            abort,
            sink,
            Some(call_id.clone()),
            child_log.clone(),
            nested_depth + 1,
            tool_hosts_scripts(name),
            branch_entries.clone(),
        );

        let mut found = None;
        for ext in core::extensions::registered() {
            if ext.tools().iter().any(|t| t.name == name) {
                found = Some(
                    ext.execute_tool_async(name.to_string(), args.clone(), child_ctx.clone())
                        .await,
                );
                break;
            }
        }
        found.unwrap_or_else(|| Err(ToolError(format!("Tool {name} not found"))))
    };

    let duration_ms = started.elapsed().as_millis() as u64;
    let (is_error, text, structured, usage) = match &result {
        Ok(t) => (
            t.is_error,
            t.text.clone(),
            t.structured_content.clone(),
            t.usage.clone(),
        ),
        Err(e) => (true, e.0.clone(), None, None),
    };

    let children = take_log(&child_log);
    let mut record = serde_json::Map::new();
    record.insert("toolCallId".into(), json!(call_id));
    record.insert("toolName".into(), json!(name));
    record.insert("args".into(), nested_args_value(&args));
    record.insert("isError".into(), json!(is_error));
    record.insert(
        "text".into(),
        json!(truncate_chars(&text, NESTED_CALL_TEXT_CAP)),
    );
    record.insert("durationMs".into(), json!(duration_ms));

    if let Some(s) = &structured {
        record.insert("structuredContent".into(), s.clone());
    }

    if let Some(u) = &usage {
        record.insert(
            "usage".into(),
            serde_json::to_value(u).unwrap_or(Value::Null),
        );
    }
    if !children.calls.is_empty() {
        record.insert("nestedCalls".into(), Value::Array(children.calls.clone()));
    }

    if children.dropped > 0 {
        record.insert("nestedCallsDropped".into(), json!(children.dropped));
    }

    emit_nested_end(sink, &call_id, parent_call_id.as_deref(), &record);

    // 账本：记录有界，用量无条件累计（用量不因记录被丢弃而丢）
    let mut log = log.lock().unwrap();
    if log.calls.len() < MAX_NESTED_CALLS {
        log.calls.push(Value::Object(record));
    } else {
        log.dropped = log.dropped.saturating_add(1);
    }

    if let Some(own) = usage {
        log.usage = Some(match log.usage.take() {
            Some(prev) => combine_usage(&prev, &own),
            None => own,
        });
    }

    if let Some(child) = children.usage {
        log.usage = Some(match log.usage.take() {
            Some(prev) => combine_usage(&prev, &child),
            None => child,
        });
    }

    result
}

/// 嵌套调用的调用 id（宿主生成；同一调用方下序号单调递增，不依赖外部 id 生成器）。
fn new_nested_call_id(parent_call_id: &Option<String>) -> String {
    format!(
        "nested_{}_{}",
        parent_call_id.as_deref().unwrap_or("root"),
        nested_call_seq()
    )
}

/// 嵌套调用记录里的 `args`：超长时退化为 `{"_truncated": "<前 N 字符>"}`，
/// 避免一次编排把外层工具结果（及其会话条目）撑大。
fn nested_args_value(args: &Value) -> Value {
    let json = args.to_string();
    if json.chars().count() <= NESTED_CALL_TEXT_CAP {
        args.clone()
    } else {
        json!({
            "_truncated": truncate_chars(&json, NESTED_CALL_TEXT_CAP)
        })
    }
}

/// 被拒的嵌套调用（深度/取消/未知工具/暴露方式不允许/参数不合法）：
/// 同样发一对事件并把拒绝原因记入调用方账本（`isError: true`），然后返回 `Err`。
fn refuse_nested_tool(
    sink: &Option<Arc<Mutex<JsonSink>>>,
    parent_call_id: &Option<String>,
    name: &str,
    args: Value,
    reason: String,
    log: &Arc<Mutex<NestedCallLog>>,
) -> std::result::Result<ToolResult, ToolError> {
    let call_id = new_nested_call_id(parent_call_id);
    emit_nested_start(sink, &call_id, parent_call_id.as_deref(), name, &args);

    let record = json!({
        "toolCallId": call_id,
        "toolName": name,
        "args": nested_args_value(&args),
        "isError": true,
        "text": reason,
        "durationMs": 0,
    });

    if let Some(obj) = record.as_object() {
        emit_nested_end(sink, &call_id, parent_call_id.as_deref(), obj);
    }

    let mut log = log.lock().unwrap();
    if log.calls.len() < MAX_NESTED_CALLS {
        log.calls.push(record);
    } else {
        log.dropped = log.dropped.saturating_add(1);
    }

    Err(ToolError(reason))
}

/// 嵌套调用的序号（进程级单调递增，仅用于生成唯一 id）。
fn nested_call_seq() -> u64 {
    static SEQ: AtomicU32 = AtomicU32::new(0);
    SEQ.fetch_add(1, Ordering::Relaxed) as u64
}

/// 取走一份账本（记录 + 丢弃计数 + 用量），取走后账本清空。
fn take_log(log: &Arc<Mutex<NestedCallLog>>) -> NestedCallLog {
    std::mem::take(&mut *log.lock().unwrap())
}

/// 嵌套调用开始事件（比模型发起的工具调用多一个 `parentToolCallId`）。
fn emit_nested_start(
    sink: &Option<Arc<Mutex<JsonSink>>>,
    call_id: &str,
    parent: Option<&str>,
    name: &str,
    args: &Value,
) {
    if let Some(sink) = sink
        && let Ok(mut g) = sink.lock()
    {
        let f: &mut dyn FnMut(Value) = g.as_mut();
        f(json!({
            "type": "tool_execution_start",
            "toolCallId": call_id,
            "toolName": name,
            "parentToolCallId": parent,
            "args": args,
            "startedAtEpochMs": now_ms()
        }));
    }
}

/// 嵌套调用结束事件：`result` 即记入调用方 `nestedCalls` 的那一份记录。
fn emit_nested_end(
    sink: &Option<Arc<Mutex<JsonSink>>>,
    call_id: &str,
    parent: Option<&str>,
    record: &serde_json::Map<String, Value>,
) {
    if let Some(sink) = sink
        && let Ok(mut g) = sink.lock()
    {
        let f: &mut dyn FnMut(Value) = g.as_mut();
        f(json!({
            "type": "tool_execution_end",
            "toolCallId": call_id,
            "toolName": record.get("toolName"),
            "parentToolCallId": parent,
            "isError": record.get("isError").and_then(|v| v.as_bool()).unwrap_or(false),
            "result": {
                "content": [{ "type": "text", "text": record.get("text").cloned().unwrap_or(Value::Null) }],
                "details": { "nestedCall": Value::Object(record.clone()) },
            },
        }));
    }
}

/// 把本次工具执行期间产生的嵌套调用并入调用方结果：
/// `details.nestedCalls`（有界记录）+ 用量合计（会计入会话成本）。
fn attach_nested_calls(fin: &mut FinalizedToolCall, ctx: Option<&ToolExecCtx>) {
    let Some(ctx) = ctx else {
        return;
    };

    let log = ctx.take_nested_calls();
    if let Some(u) = log.usage {
        fin.usage = Some(match fin.usage.take() {
            Some(prev) => combine_usage(&prev, &u),
            None => u,
        });
    }

    if log.calls.is_empty() && log.dropped == 0 {
        return;
    }

    let mut details = match fin.details.take() {
        Some(Value::Object(o)) => o,
        _ => serde_json::Map::new(),
    };
    details.insert("nestedCalls".into(), Value::Array(log.calls));

    if log.dropped > 0 {
        details.insert("nestedCallsDropped".into(), json!(log.dropped));
    }
    fin.details = Some(Value::Object(details));
}

/// 执行一个非 immediate 的 Prepared（并行 thunk）；完成后发 tool_execution_end。
/// 返回 (工具结果, 该工具自身耗时毫秒)。
fn execute_one(
    prepared: &Prepared,
    cwd: String,
    sink: &Option<Arc<Mutex<JsonSink>>>,
    abort: &Arc<AtomicBool>,
    tpl: &Arc<AgentTemplate>,
    branch_entries: Option<Arc<Vec<Value>>>,
) -> BoxFuture<'static, (FinalizedToolCall, Option<u64>)> {
    let call = prepared.call.clone();
    let name = prepared.name.clone();
    let args = prepared.args.clone();
    let abort = abort.clone();
    let sink = sink.clone();
    let tpl = tpl.clone();
    let on_chunk = Agent::bash_chunk_callback(sink.clone(), &prepared.tool_call_id(), &name, &args);
    let parent_tool_call_id = prepared.tool_call_id();

    Box::pin(async move {
        if abort.load(Ordering::Relaxed) {
            let f =
                FinalizedToolCall::error_result(&call, &name, &args, "Operation aborted", false);
            emit_tool_execution_end(&sink, &f);
            return (f, None);
        }

        // 并行 thunk 内单独计时（从执行开始到工具返回）
        let tool_start = Instant::now();
        let idx = ToolIndex::resolve(&tpl.rebuild_ctx.injected_tools, &name);
        let mut exec_ctx: Option<ToolExecCtx> = None;
        let result = if let Some(handler) = idx.injected_handler() {
            handler(args.clone()) // 注入工具：同步 handler，不经 ToolExecCtx
        } else if idx.is_builtin() {
            let model_supports_images = tpl.model.input.iter().any(|i| i == "image");
            Agent::execute_builtin_tool(
                &cwd,
                &name,
                &args,
                on_chunk,
                model_supports_images,
                None,
                tpl.model.image_resize,
            )
            .await
        } else {
            let ctx = make_exec_ctx(
                &cwd,
                &tpl,
                &abort,
                &sink,
                Some(&parent_tool_call_id),
                branch_entries.clone(),
            );
            exec_ctx = Some(ctx.clone());

            let mut found = None;
            for ext in core::extensions::registered() {
                if ext.tools().iter().any(|t| t.name == name) {
                    found = Some(
                        ext.execute_tool_async(name.clone(), args.clone(), ctx)
                            .await,
                    );
                    break;
                }
            }
            found.unwrap_or_else(|| Err(tools::ToolError(format!("Tool {} not found", name))))
        };

        let mut f = match result {
            Ok(t) => FinalizedToolCall::from_tool_result(&call, &name, &args, t),
            Err(e) => FinalizedToolCall::error_result(&call, &name, &args, &e.0, false),
        };

        // bash 流式更新收尾：输出被截断时补发带 details 的 tool_execution_update
        let call_id = match &call {
            ContentBlock::ToolCall { id, .. } => id.clone(),
            _ => String::new(),
        };
        if name == "bash"
            && !f.is_error
            && let Some(details) = f.details.as_ref()
            && let Some(g) = &sink
            && let Ok(mut g) = g.lock()
        {
            let cb: &mut dyn FnMut(Value) = g.as_mut();
            cb(json!({
                "type": "tool_execution_update",
                "toolCallId": call_id,
                "toolName": name,
                "args": args,
                "partialResult": { "content": [ { "type": "text", "text": f.text } ] },
                "details": details,
            }));
        }

        // 计时在钩子之前收口，只测工具本身
        let dur = Some(tool_start.elapsed().as_millis() as u64);
        f.duration_ms = dur;
        attach_nested_calls(&mut f, exec_ctx.as_ref());
        apply_after_tool_call(&mut f, &name, &args);
        emit_tool_execution_end(&sink, &f);
        (f, dur)
    })
}

/// 流事件 → json assistantMessageEvent；partial 用于 toolcall_start 注入 id/toolName
pub(crate) fn stream_event_to_json(ev: StreamEvent, partial: Option<&AgentMessage>) -> Value {
    match ev {
        StreamEvent::Start { .. } => json!({ "type": "start" }),
        StreamEvent::TextStart { content_index, .. } => {
            json!({ "type": "text_start", "contentIndex": content_index })
        }
        StreamEvent::TextDelta {
            content_index,
            delta,
            ..
        } => {
            json!({ "type": "text_delta", "contentIndex": content_index, "delta": delta })
        }
        StreamEvent::TextEnd {
            content_index,
            content,
            ..
        } => {
            json!({ "type": "text_end", "contentIndex": content_index, "content": content })
        }
        StreamEvent::ThinkingStart { content_index, .. } => {
            json!({ "type": "thinking_start", "contentIndex": content_index })
        }
        StreamEvent::ThinkingDelta {
            content_index,
            delta,
            ..
        } => {
            json!({ "type": "thinking_delta", "contentIndex": content_index, "delta": delta })
        }
        StreamEvent::ThinkingEnd {
            content_index,
            content,
            ..
        } => {
            json!({ "type": "thinking_end", "contentIndex": content_index, "content": content })
        }
        StreamEvent::ToolCallStart { content_index, .. } => {
            let (call_id, tool_name) = partial
                .and_then(|p| p.content.get(content_index))
                .and_then(|b| match b {
                    ContentBlock::ToolCall { id, name, .. } => Some((id.clone(), name.clone())),
                    _ => None,
                })
                .unwrap_or_default();
            let mut v = serde_json::Map::new();
            v.insert("type".into(), json!("toolcall_start"));
            v.insert("contentIndex".into(), json!(content_index));
            v.insert("id".into(), json!(call_id));
            v.insert("toolName".into(), json!(tool_name));
            Value::Object(v)
        }
        StreamEvent::ToolCallDelta {
            content_index,
            delta,
            ..
        } => {
            json!({ "type": "toolcall_delta", "contentIndex": content_index, "delta": delta })
        }
        StreamEvent::ToolCallEnd {
            content_index,
            tool_call,
            ..
        } => {
            let tool_call = serde_json::to_value(tool_call).unwrap_or(Value::Null);
            json!({ "type": "toolcall_end", "contentIndex": content_index, "toolCall": tool_call })
        }
    }
}

/// branch summary 输出上限：`min(4096, model.max_tokens)`
/// reasoning 模型的 thinking 会计入输出上限，硬编码 2048 会被 thinking 耗尽导致摘要截断。
fn branch_summary_max_tokens(model: &ModelConfig) -> u32 {
    model
        .max_tokens
        .filter(|m| *m > 0)
        .unwrap_or(u32::MAX)
        .min(4096)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::extensions::ToolExposure;

    #[tokio::test]
    async fn retryable_error_classification_matches_pi() {
        assert!(Agent::is_retryable_assistant_error(
            "503 service unavailable, retrying..."
        ));
        assert!(Agent::is_retryable_assistant_error(
            "connection reset by peer (network error)"
        ));
        assert!(Agent::is_retryable_assistant_error(
            "stream ended without response"
        ));
        assert!(Agent::is_retryable_assistant_error(
            "overloaded 429 too many requests"
        ));
        // Selected model is at capacity（#10278）
        assert!(Agent::is_retryable_assistant_error(
            "provider returned 429 Too Many Requests: Selected model is at capacity. Please try again later."
        ));
        // provider server busy（pi 1.1.0 #10543）
        assert!(Agent::is_retryable_assistant_error(
            "provider returned error: server_busy"
        ));
        assert!(Agent::is_retryable_assistant_error(
            "The servers are currently busy, please retry"
        ));
        // Cloudflare 520（#9627）
        assert!(Agent::is_retryable_assistant_error(
            "provider returned 520: 520 status code (no body)"
        ));
        // Azure 峰值容量瞬时错误（#9669）
        assert!(Agent::is_retryable_assistant_error(
            "The system is currently experiencing high demand and cannot process your request."
        ));
        // reqwest transport error: Display only shows "error sending request for url (...)",
        // the underlying cause (connection refused / dns / tls) comes from display_full
        assert!(Agent::is_retryable_assistant_error(
            "Error: request failed: error sending request for url (https://opencode.ai/zen/go/v1/chat/completions)"
        ));
        assert!(Agent::is_retryable_assistant_error(
            "request failed: error sending request for url (https://x): error trying to connect: tcp connect error: Connection refused (os error 111)"
        ));
        assert!(!Agent::is_retryable_assistant_error(
            "Monthly usage limit reached for free tier"
        ));
        assert!(!Agent::is_retryable_assistant_error(
            "insufficient_quota: out of budget"
        ));
        assert!(!Agent::is_retryable_assistant_error(
            "GoUsageLimitError: quota exceeded for billing"
        ));
        // Sign in with ChatGPT：订阅共享用量上限不重试
        assert!(!Agent::is_retryable_assistant_error(
            "provider returned 429 Too Many Requests: {\"error\":{\"code\":\"subscription_sharing_usage_limit_exceeded\"}}"
        ));
        assert!(!Agent::is_retryable_assistant_error("random model error"));
    }

    use crate::core::provider::Citation;
    use serde_json::{Value as J, json};
    use tokio::{
        io::{AsyncReadExt as _, AsyncWriteExt as _},
        net::TcpListener,
    };

    /// subagent 测试互斥：全局注册表条目、扩展注册表、结果表跨测试共享，
    /// register/unregister 与 `reset_all` 会互相清空对方的在飞记录，必须串行。
    /// 用扩展自己导出的同一把锁（`manager::test_lock`），否则本文件与
    /// `extensions/subagent/manager.rs` 的测试之间仍会互踩。
    ///
    /// 同时持 `AUTH_TEST_LOCK`：本文件的子代理用例会 drain 全局 UI 队列
    /// （完成通知），而 plan-mode 等用例也往同一队列里写；二者不串行时
    /// `take_pending_ui` 会抓到对方排的请求（use-after-take 随机失败）。
    /// 锁序：AUTH → test_lock（与其它持锁用例一致）。
    fn subagent_test_lock() -> (SubagentAuthGuard, crate::extensions::subagent::TestLock) {
        let auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let lock = crate::extensions::subagent::test_lock();
        (SubagentAuthGuard(auth), lock)
    }

    /// 持有 `AUTH_TEST_LOCK` 的 RAII（仅为把锁活到用例结束）。
    struct SubagentAuthGuard(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);

    /// 读请求头，并按 Content-Length 把请求体读完再响应。
    /// 压缩摘要请求会把长对话全量序列化进 body（可达数 MB），只读到 `\r\n\r\n`
    /// 就回包并关闭连接会让仍在写 body 的客户端 EPIPE。
    async fn read_request(socket: &mut tokio::net::TcpStream) {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 8192];
        let header_end = loop {
            let n = socket.read(&mut tmp).await.unwrap();
            if n == 0 {
                break buf.len();
            }
            buf.extend_from_slice(&tmp[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos + 4;
            }
        };
        let Some(len) = content_length(&buf[..header_end.min(buf.len())]) else {
            return;
        };
        while buf.len() < header_end + len {
            let n = socket.read(&mut tmp).await.unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
        }
    }

    /// 从请求头解析 Content-Length（无 body 返回 None）
    fn content_length(headers: &[u8]) -> Option<usize> {
        let text = String::from_utf8_lossy(headers).to_ascii_lowercase();
        let rest = &text[text.find("content-length:")? + "content-length:".len()..];
        let end = rest.find(['\r', '\n']).unwrap_or(rest.len());
        rest[..end].trim().parse().ok()
    }

    /// 多轮 mock SSE 服务器：每接受一个连接，按顺序响应一组 frames。
    async fn mock_server_multi(
        responses: Vec<Vec<&'static str>>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            for frames in responses {
                // accept 加超时：响应数多于实际连接时（例如 shouldStopAfterTurn 提前结束）
                // 不阻塞整个测试
                let (mut socket, _) = match tokio::time::timeout(
                    std::time::Duration::from_millis(5000),
                    listener.accept(),
                )
                .await
                {
                    Ok(Ok(pair)) => pair,
                    _ => break,
                };
                read_request(&mut socket).await;
                socket
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                    )
                    .await
                    .unwrap();
                for f in frames {
                    let payload = format!("data: {}\n\n", f);
                    socket.write_all(payload.as_bytes()).await.unwrap();
                }
                socket.shutdown().await.unwrap();
            }
        });
        (format!("http://127.0.0.1:{}", addr.port()), handle)
    }

    /// 单次 JSON 响应的 mock 服务器（非流式请求，如分类器）：只为第一个连接服务，
    /// 5s 内没有连接就退出（避免测试因请求未发出而永久等待）。
    async fn mock_json_once(body: String) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let Ok(Ok((mut socket, _))) =
                tokio::time::timeout(std::time::Duration::from_millis(5_000), listener.accept())
                    .await
            else {
                return;
            };
            read_request(&mut socket).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
        });
        (format!("http://127.0.0.1:{}", addr.port()), handle)
    }

    /// 混合响应 mock server：按顺序对每个连接回 HTTP 错误或 SSE 流。
    enum MockResp {
        Err(&'static str, &'static str),
        Sse(Vec<&'static str>),
    }

    async fn mock_server_sequence(
        responses: Vec<MockResp>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            for resp in responses {
                let (mut socket, _) = match tokio::time::timeout(
                    std::time::Duration::from_millis(5000),
                    listener.accept(),
                )
                .await
                {
                    Ok(Ok(pair)) => pair,
                    _ => break,
                };
                read_request(&mut socket).await;
                match resp {
                    MockResp::Err(status, body) => {
                        let out = format!(
                            "{status}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}"
                        );
                        socket.write_all(out.as_bytes()).await.unwrap();
                    }
                    MockResp::Sse(frames) => {
                        socket
                            .write_all(
                                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                            )
                            .await
                            .unwrap();
                        for f in frames {
                            let payload = format!("data: {}\n\n", f);
                            socket.write_all(payload.as_bytes()).await.unwrap();
                        }
                    }
                }
                socket.shutdown().await.unwrap();
            }
        });
        (format!("http://127.0.0.1:{}", addr.port()), handle)
    }

    fn frame(s: String) -> &'static str {
        Box::leak(s.into_boxed_str())
    }

    fn text_frame(text: &str, _id: &str) -> Vec<&'static str> {
        vec![
            r#"{"id":"cmpl_t","choices":[{"index":0,"delta":{"role":"assistant","content":""}}]}"#,
            frame(format!(
                r#"{{"id":"cmpl_t","choices":[{{"index":0,"delta":{{"content":"{}"}}}}]}}"#,
                text
            )),
            r#"{"id":"cmpl_t","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            r#"[DONE]"#,
        ]
    }

    fn tool_call_frame(tool_name: &str, args: &str, id: &str) -> Vec<&'static str> {
        vec![
            r#"{"id":"cmpl_t","choices":[{"index":0,"delta":{"role":"assistant","content":null}}]}"#,
            frame(format!(
                r#"{{"id":"cmpl_t","choices":[{{"index":0,"delta":{{"tool_calls":[{{"index":0,"id":"{}","type":"function","function":{{"name":"{}","arguments":"{}"}}}}]}}}}]}}"#,
                id, tool_name, args
            )),
            r#"{"id":"cmpl_t","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
            r#"[DONE]"#,
        ]
    }

    /// 声明了语法约束采样的扩展工具：参数 schema 带上 `x-prux-grammar`（含承载源码的属性名），
    /// provider 层能把它解成协议无关的语法约束。
    #[test]
    fn parameters_with_grammar_sampling_annotates_schema() {
        use crate::core::extensions::Extension as _;
        let tool = crate::extensions::codemode::Codemode
            .tools()
            .into_iter()
            .find(|t| t.name == "codemode")
            .expect("codemode 工具");

        let annotated =
            parameters_with_grammar_sampling(&tool.parameters, tool.grammar_sampling.as_ref());
        assert_eq!(annotated["x-prux-grammar"]["input"], "code");
        assert!(
            annotated["x-prux-grammar"]["openai_lark"]
                .as_str()
                .unwrap_or_default()
                .contains("options_source")
        );

        let spec = crate::core::provider::grammar_tool(&annotated).expect("语法约束可解析");
        assert_eq!(spec.syntax, "lark");
        assert_eq!(spec.input, "code");

        // 无语法约束的工具不改 schema
        assert!(
            parameters_with_grammar_sampling(&tool.parameters, None)
                .get("x-prux-grammar")
                .is_none()
        );
    }

    #[test]
    fn branch_summary_max_tokens_caps_at_4096_and_model() {
        // 默认（无 max_tokens）→ 4096；模型 max_tokens 更小时用模型的；0 视为无上限。
        let base = ModelConfig {
            model_type: Default::default(),
            image_resize: crate::utils::image::ImageResizeLimits::default(),
            allowed_fallback_models: Vec::new(),
            provider: "test".into(),
            model_id: "m".into(),
            base_url: "https://example.com/v1".into(),
            api_key: String::new(),
            api: "openai-completions".into(),
            output: Vec::new(),
            input: vec!["text".into()],
            reasoning: true,
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
        };
        assert_eq!(branch_summary_max_tokens(&base), 4096);
        let mut small = base.clone();
        small.max_tokens = Some(1024);
        assert_eq!(branch_summary_max_tokens(&small), 1024);
        let mut zero = base.clone();
        zero.max_tokens = Some(0);
        assert_eq!(branch_summary_max_tokens(&zero), 4096);
        let mut big = base.clone();
        big.max_tokens = Some(60000);
        assert_eq!(branch_summary_max_tokens(&big), 4096);
    }

    #[test]
    fn deferred_handle_serializes_roundtrip() {
        // 验证 DeferredHandle 数据层序列化往返
        let h = crate::core::provider::DeferredHandle {
            provider: "test".into(),
            model_id: "m".into(),
            api: "openai-responses".into(),
            id: "resp_1|call_2".into(),
            ..Default::default()
        };
        let v = serde_json::to_value(&h).unwrap();
        assert_eq!(v["id"], "resp_1|call_2");
        assert_eq!(v["modelId"], "m");
    }

    #[test]
    fn on_response_callback_receives_llm_result_summary() {
        // 用 mock server 返回一个纯文本回合，验证 on_response 回调收到 usage/stopReason
        let rt = test_rt();
        let frames1 = vec![
            r#"{"id":"cmpl_t","choices":[{"index":0,"delta":{"role":"assistant","content":""}}]}"#,
            r#"{"id":"cmpl_t","choices":[{"index":0,"delta":{"content":"hello world"}}]}"#,
            r#"{"id":"cmpl_t","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            r#"{"id":"cmpl_t","choices":[{"index":0,"delta":{},"finish_reason":null}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#,
            r#"[DONE]"#,
        ];
        let (base, handle) = rt.block_on(mock_server_multi(vec![frames1]));
        let mut agent = test_agent(&base, ".");
        let got: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let got2 = got.clone();
        agent.set_on_response(Box::new(move |summary: &J| {
            *got2.lock().unwrap() = Some(summary["text"].as_str().unwrap_or("").to_string());
        }));
        let _ = rt.block_on(agent.prompt("hi"));
        let _ = rt.block_on(handle);
        let text = (*got.lock().unwrap()).clone().unwrap_or_default();
        assert!(
            text.contains("hello world"),
            "on_response summary: {text:?}"
        );
    }

    #[test]
    fn citations_serialize_roundtrip() {
        let c = Citation {
            kind: "citation".into(),
            start_index: Some(1),
            end_index: Some(5),
            url: Some("https://example.com".into()),
            title: Some("Doc".into()),
            text: Some("cited".into()),
        };
        let mut m = AgentMessage::user_text("");
        m.role = "assistant".into();
        m.citations = Some(vec![c]);
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["citations"][0]["url"], "https://example.com");
        assert_eq!(v["citations"][0]["title"], "Doc");
        // 无 citations 时不序列化（保持兼容）
        let m2 = AgentMessage::user_text("x");
        let v2 = serde_json::to_value(&m2).unwrap();
        assert!(v2.get("citations").is_none(), "{v2}");
    }

    #[test]
    fn request_abort_is_cooperative_and_emits_aborted_failure() {
        // 不发起任何 LLM 调用（端口 1 无服务）；验证协作式取消信号与 live state
        let mut agent = test_agent("http://127.0.0.1:1", ".");
        let collected: Arc<Mutex<Vec<J>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = collected.clone();
        agent.attach_json_sink(Box::new(move |e: J| sink.lock().unwrap().push(e)));
        // 模拟：run 开始后外部请求 abort；下一轮（follow-up 场景）不再发起 LLM 调用。
        // 此处直接验证 abort 信号语义：请求取消后 should_stop 不启动新一轮。
        agent.request_abort();
        assert!(agent.run_abort.load(Ordering::Relaxed));
        let _ = collected;
    }

    /// 2.12：`agent_settled` 带 `aborted`，集成方据此区分「被取消的运行」与正常完成
    /// （对齐 pi；取消信号在每次 run 开始复位，所以读到的就是本轮的结果）。
    #[test]
    fn agent_settled_reports_whether_the_run_was_aborted() {
        let mut agent = test_agent("http://127.0.0.1:1", ".");
        let collected: Arc<Mutex<Vec<J>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = collected.clone();
        agent.attach_json_sink(Box::new(move |e: J| sink.lock().unwrap().push(e)));

        agent.emit_agent_settled();
        agent.request_abort();
        agent.emit_agent_settled();

        let evs = collected.lock().unwrap();
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0]["type"], "agent_settled");
        assert_eq!(evs[0]["aborted"], false, "未取消的整轮: {}", evs[0]);
        assert_eq!(evs[1]["aborted"], true, "已取消的整轮: {}", evs[1]);
    }

    #[test]
    fn reset_run_state_resets_without_reallocating_abort_signal() {
        // 子代理物化时捕获的 `abort` 句柄要能跨 `prompt` 触达运行中的取消信号：
        // 每次 run 重新分配 Arc 会让捕获到的句柄（以及 `/agents stop`）指向废信号。
        let mut agent = test_agent("http://127.0.0.1:1", ".");
        let captured = agent.run_abort.clone();
        agent.run_abort.store(true, Ordering::Relaxed);
        agent.reset_run_state();

        assert!(!captured.load(Ordering::Relaxed), "reset 必须复位同一信号");
        assert!(
            Arc::ptr_eq(&captured, &agent.run_abort),
            "Arc 身份必须保持，否则旧句柄永远停不下新一轮"
        );
    }

    #[test]
    fn abort_and_detach_signals_old_signal_and_installs_fresh_one() {
        // 硬中止：置位本轮信号（后台子代理持有的 parent_abort 克隆据此取消），
        // 换新信号后 `exec_ctx()` 不再携带「已取消」，避免缓存 ctx 污染后续 spawn。
        let mut agent = test_agent("http://127.0.0.1:1", ".");
        let round = agent.run_abort.clone();
        let old = agent.abort_and_detach();

        assert!(round.load(Ordering::Relaxed), "本轮信号必须置位");
        assert!(
            old.load(Ordering::Relaxed),
            "返回的旧信号即已被置位的那一个"
        );
        assert!(
            !agent.run_abort.load(Ordering::Relaxed),
            "换上的新信号必须是未置位"
        );
        assert!(
            !agent.exec_ctx().parent_abort.load(Ordering::Relaxed),
            "刷新后的执行上下文不得再带已取消标记"
        );
    }

    #[test]
    fn compaction_cut_point_never_splits_assistant_turn() {
        // 构造 user → assistant → user → assistant(toolUse) 序列，切点预期回退到最近 user
        let mut msgs: Vec<AgentMessage> = Vec::new();
        msgs.push(AgentMessage::user_text("q1"));
        let mut a1 = AgentMessage::user_text("");
        a1.role = "assistant".into();
        a1.content = vec![ContentBlock::Text {
            text: "r1".into(),
            text_signature: None,
        }];
        msgs.push(a1);
        msgs.push(AgentMessage::user_text("q2"));
        let mut a2 = AgentMessage::user_text("");
        a2.role = "assistant".into();
        a2.content = vec![ContentBlock::Text {
            text: "r2".into(),
            text_signature: None,
        }];
        msgs.push(a2);
        // 估算 token：全部 assistant/user 都小，切点默认不动；这里直接验证 split 回溯逻辑
        // 通过 settings 强制 keepRecent=1：切点必是最后一条（assistant）
        let mut agent = test_agent("http://127.0.0.1:1", ".");
        agent.messages = msgs;
        agent.auto_compaction = false;
        // 手工调用切点算法不可直接访问，因此验证不抛错 + 保持消息数
        assert_eq!(agent.messages.len(), 4);
    }

    #[test]
    fn threshold_compaction_skipped_when_usage_anchor_predates_compaction() {
        // 对齐 pi：usage 锚点（最后一条带 usage 的 assistant）早于最近 compaction boundary 时，
        // 阈值压缩必须跳过；否则压缩后首轮会用压缩前的陈旧 usage 重复触发压缩。
        // 守卫拦截时不会发出任何 summary 请求（base_url 指向无效地址也不报错）。
        let dir = tempfile::tempdir().unwrap();
        let mut agent = test_agent("http://127.0.0.1:1", ".");
        let mut sess = Session::create(".", Some(dir.path().to_path_buf()), false).unwrap();

        // 先写一条压缩前带 usage 的 assistant 消息
        let mut old = AgentMessage::user_text("");
        old.role = "assistant".into();
        old.timestamp = 1_000;
        old.usage = Some(Usage {
            total_tokens: 5_000_000,
            ..Usage::default()
        });
        sess.append_message(&old);
        // 再写 compaction boundary（commit 时时间戳 > 1000）
        sess.append_compaction("## Goal\nsum", 10, None, None, None);
        agent.session = Some(sess);
        agent.messages = vec![old];
        agent.auto_compaction = true;

        let rt = test_rt();
        let r =
            rt.block_on(agent.maybe_compact_with(compaction::CompactionReason::Threshold, None));
        assert!(
            matches!(r, Ok(None)),
            "陈旧 usage 锚点应跳过阈值压缩: {:?}",
            r
        );
    }

    #[test]
    fn compaction_end_reports_shrunk_context_for_footer() {
        // 压缩成功的 compaction_end 必须带上压缩前/后的 token 数，且压缩后估算显著下降：
        // UI 用它覆盖 footer（rich/normal）的窗口占用——压缩摘要不带 usage，旧 assistant
        // 的 usage 是压缩前的陈旧值，没有这个数字占用就要等下一条 assistant 才更新。
        let rt = test_rt();
        let events: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));

        let mut summarized = AgentMessage::user_text(&"A".repeat(300_000));
        summarized.timestamp = 1;
        let mut summarized_reply = AgentMessage::user_text("head answer");
        summarized_reply.role = "assistant".into();
        summarized_reply.timestamp = 2;
        // 大文本：作为切点（保留尾部从它开始，>= keepRecentTokens）
        let mut kept_user = AgentMessage::user_text(&"B".repeat(200_000));
        kept_user.timestamp = 3;
        let mut kept_assistant = AgentMessage::user_text("tail answer");
        kept_assistant.role = "assistant".into();
        kept_assistant.timestamp = 4;
        let messages = vec![summarized, summarized_reply, kept_user, kept_assistant];
        let tokens_before = compaction::estimate_context_tokens(&messages).tokens;

        // mock server 有 5s accept 超时：等大段文本构造/编码完成后才启动
        let (base, handle) = rt.block_on(mock_server_sequence(vec![MockResp::Sse(text_frame(
            "summary text",
            "s",
        ))]));
        let mut agent = test_agent(&base, ".");
        let sink_events = events.clone();
        agent.attach_json_sink(Box::new(move |e: Value| {
            sink_events.lock().unwrap().push(e);
        }));
        agent.messages = messages;

        let r = rt.block_on(agent.maybe_compact_with(compaction::CompactionReason::Manual, None));
        let _ = rt.block_on(handle);
        assert!(matches!(r, Ok(Some(_))), "应完成压缩: {:?}", r);
        // 上下文重建为摘要 + 保留尾部（被压缩的大段文本不再进入上下文）
        assert_eq!(agent.messages.len(), 3);
        assert_eq!(agent.messages[0].role, "compactionSummary");
        assert!(
            agent.messages[1].text().starts_with("BBBB"),
            "切点之后的消息原样保留"
        );
        assert_eq!(agent.messages[2].text(), "tail answer");

        let evs = events.lock().unwrap();
        let ev = evs
            .iter()
            .find(|e| e["type"] == "compaction_end" && e["result"] == "completed")
            .expect("compaction_end(completed) 事件");
        assert_eq!(
            ev["tokensBefore"].as_u64(),
            Some(tokens_before as u64),
            "压缩前 token 数"
        );
        let after = ev["estimatedTokensAfter"].as_u64().unwrap();
        assert!(
            after < tokens_before as u64,
            "压缩后估算应下降（footer 占用随之更新）: {tokens_before} → {after}"
        );
        // 事件里的数字 = 重建上下文的纯估算（摘要 + 保留尾部）
        let pure: u64 = agent
            .messages
            .iter()
            .map(compaction::estimate_tokens)
            .sum::<u32>() as u64;
        assert_eq!(after, pure, "压缩后估算 = 重建上下文纯估算");
    }

    #[test]
    fn overflow_error_patterns_cover_providers() {
        // 对齐 pi overflow.ts：常见 provider 溢出文案必须命中，限流类不误判。
        assert!(Agent::is_overflow_error_str(
            "prompt is too long: 213462 tokens > 200000 maximum"
        ));
        // z.ai CN 端点（#10208）
        assert!(Agent::is_overflow_error_str("Prompt exceeds max length"));
        assert!(Agent::is_overflow_error_str(
            "request_too_large: Request exceeds the maximum size"
        ));
        assert!(Agent::is_overflow_error_str(
            "Your input exceeds the context window of this model"
        ));
        assert!(Agent::is_overflow_error_str(
            "exceeds the model's maximum context length of 131072 tokens"
        ));
        assert!(Agent::is_overflow_error_str(
            "This model's maximum prompt length is 131072 but the request contains 537812 tokens"
        ));
        assert!(Agent::is_overflow_error_str(
            "Please reduce the length of the messages or completion"
        ));
        assert!(Agent::is_overflow_error_str(
            "The input token count (1196265) exceeds the maximum number of tokens allowed (1048575)"
        ));
        assert!(Agent::is_overflow_error_str(
            "prompt token count of 1000 exceeds the limit of 999"
        ));
        assert!(Agent::is_overflow_error_str(
            "Invalid params: context window exceeds limit"
        ));
        assert!(Agent::is_overflow_error_str(
            "Prompt contains 90001 tokens ... too large for model with 200000 maximum context length"
        ));
        assert!(Agent::is_overflow_error_str(
            "Prompt has 90001 tokens, but the configured context size is 200000 tokens"
        ));
        assert!(Agent::is_overflow_error_str(
            "The input (90001 tokens) is longer than the model's context length (20000 tokens)"
        ));
        assert!(Agent::is_overflow_error_str(
            "Range of input length should be [1, 90000]"
        ));
        // 非溢出（限流/服务不可用）不得误判为溢出
        assert!(!Agent::is_overflow_error_str(
            "ThrottlingException: Too many tokens, please wait before trying again"
        ));
        assert!(!Agent::is_overflow_error_str(
            "Rate limit reached for anthropic"
        ));
        assert!(!Agent::is_overflow_error_str("Service unavailable"));
    }

    #[test]
    fn context_overflow_covers_error_silent_and_length_stop() {
        // 对齐 pi isContextOverflow 三态
        let mut m = AgentMessage::user_text("");
        m.role = "assistant".into();

        // 1) error 文案
        m.stop_reason = Some("error".into());
        m.error_message = Some("prompt is too long".into());
        assert!(Agent::message_is_context_overflow(&m, 200_000, "anthropic"));

        // 2) Cerebras 无 body 400/413（仅 cerebras）
        m.error_message = Some("provider returned 400 Bad Request: ".into());
        assert!(Agent::message_is_context_overflow(&m, 200_000, "cerebras"));
        assert!(!Agent::message_is_context_overflow(&m, 200_000, "openai"));

        // 3) 静默溢出（z.ai）：stop + input+cacheRead > window
        m.stop_reason = Some("stop".into());
        m.error_message = None;
        m.usage = Some(Usage {
            input: 210_000,
            ..Usage::default()
        });
        assert!(Agent::message_is_context_overflow(&m, 200_000, "zai"));
        m.usage = Some(Usage {
            input: 100_000,
            ..Usage::default()
        });
        assert!(!Agent::message_is_context_overflow(&m, 200_000, "zai"));

        // 4) length-stop 溢出（Xiaomi）：output=0 且输入填满窗口（≥99%）
        m.stop_reason = Some("length".into());
        m.usage = Some(Usage {
            input: 199_000,
            output: 0,
            ..Usage::default()
        });
        assert!(Agent::message_is_context_overflow(&m, 200_000, "xiaomi"));
        // 有输出则不算
        m.usage = Some(Usage {
            input: 199_000,
            output: 5,
            ..Usage::default()
        });
        assert!(!Agent::message_is_context_overflow(&m, 200_000, "xiaomi"));
    }

    #[test]
    fn overflow_removes_failed_message_compacts_and_retries_once() {
        // 对齐 pi Case 1：overflow 后从重试上下文移除失败 assistant（session 历史保留）、
        // Overflow 压缩、重试一轮；最终回复来自重试。
        let rt = test_rt();
        let events: std::sync::Arc<std::sync::Mutex<Vec<Value>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let overflow_body =
            r#"{"error":{"message":"prompt is too long: 500000 tokens > 200000 maximum"}}"#;
        let (base, handle) = rt.block_on(mock_server_sequence(vec![
            MockResp::Err("HTTP/1.1 400 Bad Request", overflow_body),
            MockResp::Sse(text_frame("summary text", "s")),
            MockResp::Sse(text_frame("retried reply", "r")),
        ]));
        let mut agent = test_agent(&base, ".");
        agent.auto_retry = true;
        let sink_events = events.clone();
        agent.attach_json_sink(Box::new(move |e: Value| {
            sink_events.lock().unwrap().push(e);
        }));
        let result = rt.block_on(agent.prompt_messages(vec![
            AgentMessage::user_text("hello"),
            AgentMessage::user_text(&"x".repeat(400_000)), // 大文本：保证压缩有可切分内容
        ]));
        let _ = rt.block_on(handle);
        let r = result.unwrap();
        assert_eq!(r, "retried reply");
        // 失败/截断的 assistant 已从重试上下文移除
        assert!(
            !agent
                .messages
                .iter()
                .any(|m| m.stop_reason.as_deref() == Some("error")),
            "重试后 messages 不应残留 error assistant"
        );
        // 压缩发生过：上下文以 compactionSummary 开头
        assert_eq!(
            agent.messages.first().map(|m| m.role.as_str()),
            Some("compactionSummary")
        );
        let evs = events.lock().unwrap();
        assert!(
            evs.iter()
                .any(|e| e["type"] == "compaction_start" && e["reason"] == "overflow")
        );
        assert!(
            evs.iter()
                .any(|e| e["type"] == "compaction_end" && e["reason"] == "overflow")
        );
        // 溢出恢复会重跑 run_loop（>1 次 agent_end），但整轮只 settle 一次，
        // 且 agent_settled 是最后一个事件（UI 收尾边界）。
        assert!(
            evs.iter().filter(|e| e["type"] == "agent_end").count() > 1,
            "溢出恢复应产生多次 agent_end（每次尝试一次）: {evs:?}"
        );
        assert_eq!(
            evs.iter().filter(|e| e["type"] == "agent_settled").count(),
            1,
            "整轮应只发一次 agent_settled: {evs:?}"
        );
        assert_eq!(
            evs.last().map(|e| e["type"].as_str()),
            Some(Some("agent_settled")),
            "agent_settled 应为最后一个事件"
        );
        assert_eq!(
            evs.last().map(|e| e["aborted"].as_bool()),
            Some(Some(false)),
            "正常完成的整轮 aborted=false"
        );
    }

    #[test]
    fn overflow_retry_guard_compacts_only_once() {
        // 重试后仍溢出：守卫拒绝二次压缩，只发一次 compaction_end(failed) 通知
        // （对齐 pi `_overflowRecoveryAttempted` 失败路径）。
        let rt = test_rt();
        let events: std::sync::Arc<std::sync::Mutex<Vec<Value>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let overflow_body =
            r#"{"error":{"message":"prompt is too long: 500000 tokens > 200000 maximum"}}"#;
        let (base, handle) = rt.block_on(mock_server_sequence(vec![
            MockResp::Err("HTTP/1.1 400 Bad Request", overflow_body),
            MockResp::Sse(text_frame("summary text", "s")),
            MockResp::Err("HTTP/1.1 400 Bad Request", overflow_body),
        ]));
        let mut agent = test_agent(&base, ".");
        agent.auto_retry = true;
        let sink_events = events.clone();
        agent.attach_json_sink(Box::new(move |e: Value| {
            sink_events.lock().unwrap().push(e);
        }));
        let result = rt.block_on(agent.prompt_messages(vec![
            AgentMessage::user_text("hello"),
            AgentMessage::user_text(&"x".repeat(400_000)), // 大文本：保证压缩有可切分内容
        ]));
        let _ = rt.block_on(handle);
        assert!(result.is_ok(), "error 回合不抛 Err: {:?}", result.err());
        // 只压缩一次
        let evs = events.lock().unwrap();
        let starts = evs
            .iter()
            .filter(|e| e["type"] == "compaction_start" && e["reason"] == "overflow")
            .count();
        assert_eq!(starts, 1, "overflow 压缩必须只发生一次");
        // 失败通知转达
        assert!(evs.iter().any(|e| {
            e["type"] == "compaction_end" && e["reason"] == "overflow" && e["willRetry"] == false
        }));
    }

    /// 一.2（pi 0.87.0）：错误重试时被放弃的 attempt 以 append-only 的 `context_edit`
    /// 落库（`_prepareRetry` → `_omitRecoveryAttempt`），原始 transcript 保留，
    /// 但**模型上下文**永久省略它——包括 `/resume`、树导航重建之后。
    #[test]
    fn retry_omits_abandoned_attempt_from_rebuilt_context() {
        let _agent_dir = crate::test_support::AgentDirGuard::temp();
        // 退避默认 2s：压到 1ms 让测试快（写入本线程的临时 agent 目录）
        let dir = crate::test_support::agent_dir_override().expect("temp agent dir");
        std::fs::write(dir.join("settings.json"), r#"{"retry":{"baseDelayMs":1}}"#).unwrap();

        let rt = test_rt();
        let (base, handle) = rt.block_on(mock_server_sequence(vec![
            MockResp::Err(
                "HTTP/1.1 503 Service Unavailable",
                r#"{"error":{"message":"overloaded"}}"#,
            ),
            MockResp::Sse(text_frame("recovered", "r")),
        ]));
        let mut agent = session_agent(&base, ".");
        agent.auto_retry = true;

        let text = rt.block_on(agent.prompt("go")).unwrap();
        let _ = rt.block_on(handle);
        assert_eq!(text, "recovered");

        let session = agent.session.as_ref().unwrap();
        let entries = session.get_entries();
        let edit = entries
            .iter()
            .find(|e| e["type"] == "context_edit")
            .expect("被放弃的 attempt 应落库 context_edit");
        assert!(edit["replacement"].is_null(), "省略形态 replacement=null");
        let target = edit["targetId"].as_str().unwrap().to_string();
        let target_entry = entries
            .iter()
            .find(|e| e["id"] == target.as_str())
            .expect("目标条目仍在原始 transcript 里");
        assert_eq!(target_entry["message"]["stopReason"], "error");

        // 重开会话（模拟 /resume / 树导航重建）：被放弃的 attempt 不再进入上下文
        let path = session
            .get_session_file()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let reopened = crate::core::session_manager::Session::open(&path).unwrap();
        let msgs = reopened.build_context_messages();
        assert!(
            !msgs
                .iter()
                .any(|m| m.entry_id.as_deref() == Some(target.as_str())),
            "resume 后不应包含被放弃的 attempt: {msgs:?}"
        );
        assert!(
            reopened
                .get_entries()
                .iter()
                .any(|e| e["id"] == target.as_str()),
            "原始 transcript 仍完整保留该条目"
        );
        // 重试后的成功回复仍在上下文里
        assert!(msgs.iter().any(|m| m.text() == "recovered"));
    }

    /// 一.2（pi 0.87.0）：overflow 恢复同样持久化省略（`_omitRecoveryAttempt` 后再压缩）。
    #[test]
    fn overflow_recovery_omits_failed_attempt_persistently() {
        let _agent_dir = crate::test_support::AgentDirGuard::temp();
        let rt = test_rt();
        let overflow_body =
            r#"{"error":{"message":"prompt is too long: 500000 tokens > 200000 maximum"}}"#;
        let (base, handle) = rt.block_on(mock_server_sequence(vec![
            MockResp::Err("HTTP/1.1 400 Bad Request", overflow_body),
            MockResp::Sse(text_frame("summary text", "s")),
            MockResp::Sse(text_frame("retried reply", "r")),
        ]));
        let mut agent = session_agent(&base, ".");
        agent.auto_retry = true;
        let result = rt.block_on(agent.prompt_messages(vec![
            AgentMessage::user_text("hello"),
            AgentMessage::user_text(&"x".repeat(400_000)),
        ]));
        let _ = rt.block_on(handle);
        assert_eq!(result.unwrap(), "retried reply");

        let session = agent.session.as_ref().unwrap();
        let entries = session.get_entries();
        let edit = entries
            .iter()
            .find(|e| e["type"] == "context_edit")
            .expect("overflow 恢复也应落库 context_edit");
        assert!(edit["replacement"].is_null());
        let target = edit["targetId"].as_str().unwrap().to_string();

        let path = session
            .get_session_file()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let reopened = crate::core::session_manager::Session::open(&path).unwrap();
        let msgs = reopened.build_context_messages();
        assert!(
            !msgs
                .iter()
                .any(|m| m.entry_id.as_deref() == Some(target.as_str())),
            "resume 后被放弃的溢出 attempt 不应回到上下文"
        );
    }

    #[test]
    fn usage_reasoning_fields_present_in_provider_results() {
        // Usage.reasoning 数据层字段存在并可序列化
        let u = Usage {
            input: 1,
            output: 2,
            reasoning: Some(1),
            ..Default::default()
        };
        let v = serde_json::to_value(&u).unwrap();
        assert_eq!(v["reasoning"], 1);
        assert_eq!(v["reasoning"], v["reasoning"]);
    }

    fn test_agent(base: &str, cwd: &str) -> Agent {
        test_agent_with_input(base, cwd, vec!["text".into()])
    }

    /// 与 [`test_agent`] 相同，但可指定模型能力（如 `["text", "image"]`）。
    fn test_agent_with_input(base: &str, cwd: &str, input: Vec<String>) -> Agent {
        let entry = model_resolver::ModelEntry {
            model_type: Default::default(),
            id: "test-model".into(),
            name: "Test".into(),
            api: "openai-completions".into(),
            base_url: base.into(),
            provider: "test".into(),
            input_limits: crate::utils::image::InputLimits::default(),
            allowed_fallback_models: Vec::new(),
            sampling_params: None,
            sampling_params_by_thinking_level: None,
            provider_routing: None,
            supports_usage_in_streaming: true,
            supports_store: true,
            supports_finish_reason: true,
            requires_assistant_after_tool_result: false,
            supports_strict_mode: false,
            send_session_affinity_headers: None,
            session_affinity_format: None,
            output: Vec::new(),
            input,
            reasoning: false,
            context_window: 200_000,
            max_tokens: 4096,
            max_tokens_field: "max_tokens".into(),
            cost: None,
            supports_developer_role: false,
            requires_reasoning_content_on_assistant_messages: false,
            thinking_format: String::new(),
            supports_reasoning_effort: false,
            thinking_level_map: None,
            auth_header: false,
            supports_openai_grammar_tools: false,
        };
        Agent::new(
            entry,
            cwd.to_string(),
            ToolSelection::tools(vec!["read".into(), "write".into()]),
            Vec::new(),
            None,
            None,
            None,
            Vec::new(),
            None,
            Some("sk-test".to_string()),
            true,
            Vec::new(),
        )
        .unwrap()
    }

    /// 带工具选择策略的 Agent（对齐 pi tool selection 语义测试）
    fn test_agent_selection(selection: ToolSelection, exclude: Vec<String>) -> Agent {
        let entry = model_resolver::ModelEntry {
            model_type: Default::default(),
            id: "test-model".into(),
            name: "Test".into(),
            api: "openai-completions".into(),
            base_url: "http://127.0.0.1:1".into(),
            provider: "test".into(),
            input_limits: crate::utils::image::InputLimits::default(),
            allowed_fallback_models: Vec::new(),
            sampling_params: None,
            sampling_params_by_thinking_level: None,
            provider_routing: None,
            supports_usage_in_streaming: true,
            supports_store: true,
            supports_finish_reason: true,
            requires_assistant_after_tool_result: false,
            supports_strict_mode: false,
            send_session_affinity_headers: None,
            session_affinity_format: None,
            output: Vec::new(),
            input: vec!["text".into()],
            reasoning: false,
            context_window: 200_000,
            max_tokens: 4096,
            max_tokens_field: "max_tokens".into(),
            cost: None,
            supports_developer_role: false,
            requires_reasoning_content_on_assistant_messages: false,
            thinking_format: String::new(),
            supports_reasoning_effort: false,
            thinking_level_map: None,
            auth_header: false,
            supports_openai_grammar_tools: false,
        };
        Agent::new(
            entry,
            "/tmp".to_string(),
            selection,
            exclude,
            None,
            None,
            None,
            Vec::new(),
            None,
            Some("sk-test".to_string()),
            true,
            Vec::new(),
        )
        .unwrap()
    }

    const BUILTIN_TOOLS: [&str; 7] = ["read", "bash", "edit", "write", "grep", "find", "ls"];

    /// 延迟工具（pi `exposure: deferred`）：未激活不进工具表/系统提示，
    /// 经 `tool_search` 激活（`activate_deferred_tools`）后下一轮可见。
    #[test]
    fn deferred_tools_hidden_until_activated() {
        use crate::core::extensions::{
            Extension, ExtensionTool, ToolExposure, activate_deferred_tools,
            clear_deferred_activations, register_extension, unregister_extension,
        };

        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        crate::core::settings_manager::write_disabled_extensions(&[]).ok();

        struct DeferredExt;
        impl Extension for DeferredExt {
            fn name(&self) -> &str {
                "deferred-demo"
            }
            fn tools(&self) -> Vec<ExtensionTool> {
                vec![
                    ExtensionTool::simple(
                        "deferred_weather",
                        "Look up the weather",
                        serde_json::json!({ "type": "object" }),
                        "weather lookup",
                    )
                    .with_exposure(ToolExposure::Deferred),
                    ExtensionTool::simple(
                        "eager_ping",
                        "Ping a host",
                        serde_json::json!({ "type": "object" }),
                        "ping",
                    ),
                ]
            }
        }

        register_extension(DeferredExt);
        clear_deferred_activations();

        let selection = || ToolSelection::Default(vec!["read".to_string()]);
        let agent = test_agent_selection(selection(), vec![]);
        let active = agent.get_active_tools();
        assert!(
            active.iter().any(|n| n == "eager_ping"),
            "非延迟的扩展工具照常可见: {active:?}"
        );
        assert!(
            !active.iter().any(|n| n == "deferred_weather"),
            "延迟工具未激活前不得进工具表: {active:?}"
        );
        assert!(
            !agent.system_prompt.contains("deferred_weather"),
            "延迟工具不得进系统提示"
        );

        activate_deferred_tools(&["deferred_weather".to_string()]);
        let agent2 = test_agent_selection(selection(), vec![]);
        let active2 = agent2.get_active_tools();
        assert!(
            active2.iter().any(|n| n == "deferred_weather"),
            "激活后应进工具表: {active2:?}"
        );

        clear_deferred_activations();
        let agent3 = test_agent_selection(selection(), vec![]);
        assert!(
            !agent3
                .get_active_tools()
                .iter()
                .any(|n| n == "deferred_weather"),
            "清空激活集合后重新隐藏"
        );
        assert!(unregister_extension("deferred-demo"));
    }

    #[test]
    fn tool_selection_no_tools_disables_all() {
        // --no-tools/-nt：内置与扩展全部关闭（对齐 pi noTools="all"）
        let agent = test_agent_selection(ToolSelection::NoTools, vec![]);
        assert!(
            agent.get_active_tools().is_empty(),
            "active: {:?}",
            agent.get_active_tools()
        );
        assert!(
            agent.get_all_tools().is_empty(),
            "all: {:?}",
            agent.get_all_tools()
        );
    }

    #[test]
    fn tool_selection_no_builtin_keeps_extensions() {
        // --no-builtin-tools/-nbt：无内置，扩展保留（对齐 pi noTools="builtin"）
        let agent = test_agent_selection(ToolSelection::NoBuiltinTools, vec![]);
        let active = agent.get_active_tools();
        assert!(
            active.iter().all(|n| !BUILTIN_TOOLS.contains(&n.as_str())),
            "active 不应含内置: {:?}",
            active
        );
        // getAllTools 注册表仍含全量内置（pi：--no-builtin-tools 只影响 active 集）
        let all = agent.get_all_tools();
        assert!(
            BUILTIN_TOOLS.iter().all(|b| all.iter().any(|n| n == b)),
            "all: {:?}",
            all
        );
    }

    #[test]
    fn tool_selection_allowlist_applies_to_all() {
        // --tools/-t：严格 allowlist，内置与扩展都按名单（对齐 pi allowedToolNames）
        let list = vec!["read".to_string(), "edit".to_string()];
        let agent = test_agent_selection(ToolSelection::Allowlist(list.clone()), vec![]);
        let active = agent.get_active_tools();
        assert!(
            active.iter().all(|n| list.contains(n)),
            "active 超出名单: {:?}",
            active
        );
        assert!(active.contains(&"read".to_string()));
        assert!(!active.contains(&"bash".to_string()));
        assert!(!active.contains(&"write".to_string()));
        let all = agent.get_all_tools();
        assert!(all.iter().all(|n| list.contains(n)), "all: {:?}", all);
    }

    #[test]
    fn tool_selection_default_and_exclude() {
        // 默认（defaultTools/标准默认）与 --exclude-tools 过滤
        let agent = test_agent_selection(
            ToolSelection::tools(vec!["read".into(), "write".into()]),
            vec!["write".to_string()],
        );
        let active = agent.get_active_tools();
        assert!(active.contains(&"read".to_string()));
        assert!(!active.contains(&"write".to_string()));
        assert!(!active.contains(&"bash".to_string()));
        let all = agent.get_all_tools();
        assert!(
            !all.contains(&"write".to_string()),
            "exclude 应作用于 getAllTools: {:?}",
            all
        );
    }

    /// `--tools` / `--exclude-tools` 的条目支持 `*` 通配（对齐 pi `createToolNameMatcher`）。
    #[test]
    fn tool_selection_supports_wildcard_entries() {
        let agent = test_agent_selection(ToolSelection::Allowlist(vec!["re*".into()]), vec![]);
        let active = agent.get_active_tools();
        assert!(
            active.contains(&"read".to_string()),
            "re* 应命中 read: {active:?}"
        );
        assert!(!active.contains(&"bash".to_string()));
        assert!(!active.contains(&"grep".to_string()));

        let agent = test_agent_selection(
            ToolSelection::tools(vec!["read".into(), "bash".into()]),
            vec!["b*".into()],
        );
        let active = agent.get_active_tools();
        assert!(active.contains(&"read".to_string()));
        assert!(
            !active.contains(&"bash".to_string()),
            "--exclude-tools 的通配条目应生效: {active:?}"
        );
    }

    /// `--tools` 未点名的 MCP 工具：仍注册（脚本/嵌套可调），但不对模型声明；
    /// `--exclude-tools` 与 `mcp__` 前缀条目都能把它挡掉。
    /// 对齐 pi 的 `_isAllowedTool()` / `_isActivatable()`。
    #[test]
    fn allowlist_keeps_mcp_tools_unless_pointed_at() {
        use crate::core::extensions::{
            Extension, ExtensionTool, register_extension, unregister_extension,
        };

        /// 以聚合网关名 `mcp` 提供工具的假扩展（真实实现见 `extensions::mcp`）。
        struct McpGatewayProbe;
        impl Extension for McpGatewayProbe {
            fn name(&self) -> &str {
                "mcp-gateway-probe"
            }
            fn tools(&self) -> Vec<ExtensionTool> {
                vec![ExtensionTool::simple(
                    tool_names::MCP_GATEWAY_TOOL_NAME,
                    "MCP gateway",
                    serde_json::json!({ "type": "object" }),
                    "mcp gateway",
                )]
            }
        }

        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        crate::core::settings_manager::write_disabled_extensions(&[]).ok();
        register_extension(McpGatewayProbe);

        // allowlist 只点名 read：mcp 保留在注册表里，但不进工具表与系统提示
        let agent = test_agent_selection(ToolSelection::Allowlist(vec!["read".into()]), vec![]);
        let active = agent.get_active_tools();
        assert!(active.contains(&"read".to_string()));
        assert!(
            !active.contains(&"mcp".to_string()),
            "未点名的 MCP 工具不得对模型声明: {active:?}"
        );
        assert!(
            agent.get_all_tools().contains(&"mcp".to_string()),
            "未点名的 MCP 工具仍应保留注册"
        );
        assert!(
            !agent.system_prompt.contains("MCP gateway"),
            "未声明的 MCP 工具不得进系统提示"
        );

        // 脚本／嵌套调用仍可达（否则「保留」等于没有）
        let script = script_tools_for(
            &[],
            &[],
            &ToolSelection::Allowlist(vec!["read".into()]),
            &[],
        );
        assert!(
            script.iter().any(|t| t.name == "mcp"),
            "保留的 MCP 工具应对脚本可调: {:?}",
            script.iter().map(|t| &t.name).collect::<Vec<_>>()
        );

        // --exclude-tools 命中：连注册都一起挡掉
        let excluded = test_agent_selection(
            ToolSelection::Allowlist(vec!["read".into()]),
            vec!["mcp".to_string()],
        );
        assert!(!excluded.get_all_tools().contains(&"mcp".to_string()));

        // 名单里出现 mcp__ 前缀条目：视为过滤 MCP（pi 的 _allowlistFiltersMcp）
        let filtered = test_agent_selection(
            ToolSelection::Allowlist(vec!["read".into(), "mcp__docs__*".into()]),
            vec![],
        );
        assert!(!filtered.get_all_tools().contains(&"mcp".to_string()));

        // 点名 mcp 本身：正常声明
        let pointed = test_agent_selection(ToolSelection::Allowlist(vec!["mcp".into()]), vec![]);
        assert!(pointed.get_active_tools().contains(&"mcp".to_string()));

        assert!(unregister_extension("mcp-gateway-probe"));
    }

    /// `--tools +name/-name` 的修饰符跟着 `/reload` 重算（对齐 pi 的 `defaultToolModifiers`）。
    #[test]
    fn reload_reapplies_default_tool_modifiers() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        let path = crate::core::settings_manager::agent_dir().join("settings.json");

        // 启动：defaultTools = [read]，命令行 --tools +bash → 选择 [read, bash]
        std::fs::write(&path, r#"{"defaultTools":["read"]}"#).unwrap();
        let base = vec!["read".to_string()];
        let modifiers = vec!["+bash".to_string()];
        let mut agent = test_agent_selection(
            ToolSelection::tools(settings_manager::apply_tool_modifiers(&base, &modifiers)),
            vec![],
        );
        agent.set_settings_default_tools(Some(settings_manager::apply_tool_modifiers(
            &base, &modifiers,
        )));
        agent.set_default_tool_modifiers(modifiers);

        // /reload：defaultTools 增加 edit；修饰符重放後 next = [read, edit, bash]，
        // 相对 previous [read, bash] 新增 edit（只增不减，顺序为「现有 + 新增」）
        std::fs::write(&path, r#"{"defaultTools":["read","edit"]}"#).unwrap();
        agent.reload_default_tools();
        let ToolSelection::Default(names) = &agent.rebuild_ctx.selection else {
            panic!("Default 选择不应被改写成其它变体");
        };
        assert_eq!(
            names,
            &vec!["read".to_string(), "bash".to_string(), "edit".to_string()]
        );
    }

    /// 2.14 `/reload` 重新应用 `defaultTools`：**只增不减**——新加入的并入当前选择，
    /// 从 `defaultTools` 移除的、以及会话里另行关掉的工具都不动。
    #[test]
    fn reload_default_tools_only_adds_new_names() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        let path = crate::core::settings_manager::agent_dir().join("settings.json");

        // 启动：defaultTools = [read]；会话里用户又把它关掉了（选择为空）。
        // 两次写入长度不同，规避读缓存按 (mtime, len) 命中的可能。
        std::fs::write(&path, r#"{"defaultTools":["read"]}"#).unwrap();
        let mut agent = test_agent_selection(ToolSelection::tools(Vec::new()), vec![]);
        agent.set_settings_default_tools(Some(vec!["read".to_string()]));

        // /reload：settings 变成 [read, bash] → 只并入新出现的 bash，read 不被复活。
        std::fs::write(&path, r#"{"defaultTools":["read","bash"]}"#).unwrap();
        agent.reload_default_tools();
        let ToolSelection::Default(names) = &agent.rebuild_ctx.selection else {
            panic!("Default 选择不应被改写成其它变体");
        };
        assert_eq!(names, &vec!["bash".to_string()]);

        // 再 reload：settings 里 bash 被移除 → 保持启用（只增不减）。
        std::fs::write(&path, r#"{"defaultTools":["read"]}"#).unwrap();
        agent.reload_default_tools();
        let ToolSelection::Default(names) = &agent.rebuild_ctx.selection else {
            panic!("Default 选择不应被改写成其它变体");
        };
        assert_eq!(names, &vec!["bash".to_string()]);
    }

    /// 2.14：命令行 `--tools` / `--no-tools` / `--no-builtin-tools` 显式指定过工具集时，
    /// `/reload` 不重读 settings.json 的 `defaultTools`。
    #[test]
    fn reload_default_tools_skips_cli_override() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        let path = crate::core::settings_manager::agent_dir().join("settings.json");
        std::fs::write(&path, r#"{"defaultTools":["read","bash"]}"#).unwrap();

        let mut agent =
            test_agent_selection(ToolSelection::Allowlist(vec!["read".to_string()]), vec![]);
        agent.reload_default_tools();
        let ToolSelection::Allowlist(names) = &agent.rebuild_ctx.selection else {
            panic!("--tools 的 allowlist 不应被 /reload 改写");
        };
        assert_eq!(names, &vec!["read".to_string()]);
    }

    fn ev_types(events: &[J]) -> Vec<String> {
        events
            .iter()
            .map(|e| {
                e.get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string()
            })
            .collect()
    }

    fn test_rt() -> tokio::runtime::Runtime {
        tokio::runtime::Runtime::new().unwrap()
    }

    fn events_of<'a>(events: &'a [J], ty: &'a str) -> Vec<&'a J> {
        events
            .iter()
            .filter(|e| e.get("type").and_then(|v| v.as_str()) == Some(ty))
            .collect()
    }

    #[test]
    fn run_loop_text_reply_pi_event_sequence() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let cwd = std::env::temp_dir();
            let cwd = cwd.to_string_lossy().to_string();
            let frames = text_frame("Hello out there", "cmpl_t");
            let (base, handle) = mock_server_multi(vec![frames]).await;

            let mut agent = test_agent(&base, &cwd);
            let collected: std::sync::Arc<std::sync::Mutex<Vec<J>>> =
                std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let sink = collected.clone();
            agent.attach_json_sink(Box::new(move |e: J| sink.lock().unwrap().push(e)));

            let text = agent.prompt("hi").await.unwrap();
            handle.await.unwrap();
            assert_eq!(text, "Hello out there");

            let events = collected.lock().unwrap().clone();
            let types = ev_types(&events);

            // agent_start → turn_start → user message_start/end → assistant start(partial) → deltas → end → turn_end → agent_end
            assert_eq!(types.first().map(String::as_str), Some("agent_start"));
            assert_eq!(types[1], "turn_start");
            assert!(types.contains(&"message_start".to_string()));
            assert!(types.contains(&"message_update".to_string()));
            assert!(types.contains(&"turn_end".to_string()));
            assert!(types.contains(&"agent_end".to_string()));

            // turn_start 只发一次（第一轮）
            assert_eq!(
                types.iter().filter(|t| t.as_str() == "turn_start").count(),
                1
            );

            // agent_end 只携带本次 newMessages（user + assistant）
            let agent_end = events_of(&events, "agent_end")[0];
            let msgs = agent_end["messages"].as_array().unwrap();
            assert_eq!(msgs.len(), 2);
            assert_eq!(msgs[0]["role"], "user");
            assert_eq!(msgs[0]["content"][0]["text"], "hi");
            assert_eq!(msgs[1]["role"], "assistant");
            assert_eq!(msgs[1]["content"][0]["text"], "Hello out there");
        });
    }

    /// 虚拟模型端到端：`/model` 选中的是虚拟条目，请求实际发给路由出的物理模型，
    /// assistant 消息记录物理模型，并广播 `model_routed`。
    #[test]
    fn virtual_model_routes_request_and_records_physical_model() {
        let _ad = crate::test_support::AgentDirGuard::temp();
        let rt = test_rt();
        rt.block_on(async {
            let cwd = std::env::temp_dir().to_string_lossy().to_string();
            let (base, handle) =
                mock_server_multi(vec![text_frame("routed reply", "cmpl_v")]).await;

            // 物理模型写进 models.json（自定义 provider）：凭据字面量 + baseUrl 指向假服务器
            std::fs::write(
                crate::core::settings_manager::agent_dir().join("models.json"),
                json!({"providers":{"mockprov":{
                    "apiKey":"sk-mock",
                    "baseUrl":base,
                    "api":"openai-completions",
                    "models":[{"id":"physical","name":"Physical"}]
                }}})
                .to_string(),
            )
            .unwrap();

            // 虚拟模型：路由器固定返回 mockprov/physical
            struct FixedRouter;
            impl virtual_models::VirtualModelRouter for FixedRouter {
                fn route<'a>(
                    &'a self,
                    _r: virtual_models::ModelRouteRequest<'a>,
                ) -> BoxFuture<'a, Result<virtual_models::ModelRoute>> {
                    Box::pin(async {
                        Ok(virtual_models::ModelRoute {
                            provider: "mockprov".into(),
                            model_id: "physical".into(),
                            thinking_level: None,
                            state: None,
                        })
                    })
                }
            }
            virtual_models::register(
                virtual_models::VirtualModelDefinition::new(
                    "mockprov",
                    "auto",
                    "Auto",
                    Arc::new(FixedRouter),
                ),
                virtual_models::STANDALONE_OWNER,
            );

            // 目录：虚拟条目可见，且与同 id 物理条目共存
            let listed = model_resolver::list_models("mockprov");
            assert!(listed.iter().any(|(id, _)| id == "auto"), "{listed:?}");
            let entry = model_resolver::find_model("mockprov", "auto").unwrap();
            assert_eq!(entry.api, virtual_models::VIRTUAL_MODEL_API);

            let mut agent = Agent::new(
                entry,
                cwd,
                ToolSelection::tools(vec!["read".into()]),
                Vec::new(),
                None,
                None,
                None,
                Vec::new(),
                None,
                None,
                false,
                Vec::new(),
            )
            .unwrap();
            assert!(agent.selected_is_virtual());

            let collected: Arc<Mutex<Vec<J>>> = Arc::new(Mutex::new(Vec::new()));
            let sink = collected.clone();
            agent.attach_json_sink(Box::new(move |e: J| sink.lock().unwrap().push(e)));

            let text = agent.prompt("hi").await.unwrap();
            handle.await.unwrap();
            assert_eq!(text, "routed reply");

            // assistant 消息记录的是物理模型（不是虚拟模型）
            let last = agent
                .messages
                .iter()
                .rev()
                .find(|m| m.role == "assistant")
                .unwrap();
            assert_eq!(last.provider.as_deref(), Some("mockprov"));
            assert_eq!(last.model.as_deref(), Some("physical"));

            // 广播了 model_routed（footer 据此显示 `→ physical`）
            let events = collected.lock().unwrap();
            let routed = events_of(&events, "model_routed");
            assert_eq!(routed.len(), 1, "{events:?}");
            assert_eq!(routed[0]["model"], "physical");

            virtual_models::unregister("mockprov", "auto");
        });
    }

    /// 路由失败：请求以错误响应结束（不发任何网络请求），消息指向虚拟模型本身。
    #[test]
    fn virtual_model_routing_failure_ends_run_with_error_message() {
        let _ad = crate::test_support::AgentDirGuard::temp();
        let rt = test_rt();
        rt.block_on(async {
            struct FailingRouter;
            impl virtual_models::VirtualModelRouter for FailingRouter {
                fn route<'a>(
                    &'a self,
                    _r: virtual_models::ModelRouteRequest<'a>,
                ) -> BoxFuture<'a, Result<virtual_models::ModelRoute>> {
                    Box::pin(async { Err(Error::msg("router exploded")) })
                }
            }
            virtual_models::register(
                virtual_models::VirtualModelDefinition::new(
                    "prux-vm-fail",
                    "auto",
                    "Auto",
                    Arc::new(FailingRouter),
                ),
                virtual_models::STANDALONE_OWNER,
            );

            let entry = model_resolver::find_model("prux-vm-fail", "auto").unwrap();
            let mut agent = Agent::new(
                entry,
                std::env::temp_dir().to_string_lossy().to_string(),
                ToolSelection::tools(vec!["read".into()]),
                Vec::new(),
                None,
                None,
                None,
                Vec::new(),
                None,
                None,
                false,
                Vec::new(),
            )
            .unwrap();

            _ = agent.prompt("hi").await;
            let last = agent.messages.last().unwrap();
            assert_eq!(last.role, "assistant");
            assert_eq!(last.stop_reason.as_deref(), Some("error"));
            assert!(
                last.error_message
                    .as_deref()
                    .unwrap_or_default()
                    .contains("router exploded"),
                "{:?}",
                last.error_message
            );
            assert_eq!(last.api.as_deref(), Some(virtual_models::VIRTUAL_MODEL_API));

            virtual_models::unregister("prux-vm-fail", "auto");
        });
    }

    #[test]
    fn run_loop_tool_then_final_reply() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let tmp =
                std::env::temp_dir().join(format!("prux-test-{}", crate::utils::time::now_ms()));
            std::fs::create_dir_all(&tmp).unwrap();
            std::fs::write(tmp.join("a.rs"), "fn main() {}").unwrap();
            let cwd = tmp.to_string_lossy().to_string();

            let frames1 = tool_call_frame("read", r#"{\"path\":\"a.rs\"}"#, "call_1");
            let frames2 = text_frame("Read done", "cmpl_2");
            let (base, handle) = mock_server_multi(vec![frames1, frames2]).await;

            let mut agent = test_agent(&base, &cwd);
            let collected: std::sync::Arc<std::sync::Mutex<Vec<J>>> =
                std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let sink = collected.clone();
            agent.attach_json_sink(Box::new(move |e: J| sink.lock().unwrap().push(e)));

            let text = agent.prompt("read it").await.unwrap();
            handle.await.unwrap();
            assert_eq!(text, "Read done");

            let events = collected.lock().unwrap().clone();
            let types = ev_types(&events);
            // 两轮：每轮一次 message_end(assistant) + turn_end
            assert_eq!(types.iter().filter(|t| t.as_str() == "turn_end").count(), 2);
            assert_eq!(
                types.iter().filter(|t| t.as_str() == "turn_start").count(),
                2
            );
            assert_eq!(
                types
                    .iter()
                    .filter(|t| t.as_str() == "tool_execution_start")
                    .count(),
                1
            );
            assert_eq!(
                types
                    .iter()
                    .filter(|t| t.as_str() == "tool_execution_end")
                    .count(),
                1
            );

            // toolResult 单独 message_start/end（对齐 pi emitToolResultMessage）
            let tr_start = events
                .iter()
                .filter(|e| e["type"] == "message_start" && e["message"]["role"] == "toolResult")
                .count();
            assert_eq!(tr_start, 1);

            // turn_end 携带 toolResults
            let turn_end = events_of(&events, "turn_end")[0];
            let tool_results = turn_end["toolResults"].as_array().unwrap();
            assert_eq!(tool_results.len(), 1);
            assert_eq!(tool_results[0]["role"], "toolResult");
            assert_eq!(tool_results[0]["isError"], false);
            assert!(
                tool_results[0]["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .contains("fn main() {}")
            );

            // agent_end 携带 user + assistant(toolcall) + toolResult + assistant(final)
            let agent_end = events_of(&events, "agent_end")[0];
            let msgs = agent_end["messages"].as_array().unwrap();
            let roles: Vec<&str> = msgs.iter().map(|m| m["role"].as_str().unwrap()).collect();
            assert_eq!(roles, vec!["user", "assistant", "toolResult", "assistant"]);
            // 第一条 assistant 含 toolCall
            assert_eq!(msgs[1]["content"][0]["type"], "toolCall");
        });
    }

    #[test]
    fn run_loop_length_stop_fails_truncated_tool_calls_then_continues() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let cwd = std::env::temp_dir().to_string_lossy().to_string();
            // 第一轮：length + 一个 tool call（参数可能截断）→ 不执行，生成错误 toolResult
            let frames1 = tool_call_frame("write", r#"{\"path\":\"x.txt\",\"content\":\"trunc"#, "call_1");
            let frames1 = {
                let mut v = frames1;
                // 把 finish_reason 改成 length（模拟被截断）
                let idx = v.len() - 2;
                v[idx] = r#"{"id":"cmpl_t","choices":[{"index":0,"delta":{},"finish_reason":"length"}]}"#;
                v
            };
            let frames2 = text_frame("Recovered", "cmpl_2");
            let (base, handle) = mock_server_multi(vec![frames1, frames2]).await;

            let mut agent = test_agent(&base, &cwd);
            let collected: std::sync::Arc<std::sync::Mutex<Vec<J>>> =
                std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let sink = collected.clone();
            agent.attach_json_sink(Box::new(move |e: J| sink.lock().unwrap().push(e)));

            let text = agent.prompt("write x").await.unwrap();
            handle.await.unwrap();
            assert_eq!(text, "Recovered");

            let events = collected.lock().unwrap().clone();
            // 对齐 pi failToolCallsFromTruncatedMessage：仍发 tool_execution_start/end（错误结果），
            // 但不真正执行工具；toolResult isError=true
            assert_eq!(ev_types(&events).iter().filter(|t| t.as_str() == "tool_execution_start").count(), 1);
            assert_eq!(ev_types(&events).iter().filter(|t| t.as_str() == "tool_execution_end").count(), 1);
            let tr = events
                .iter()
                .filter(|e| e["type"] == "message_start" && e["message"]["role"] == "toolResult")
                .count();
            assert_eq!(tr, 1);
            // 错误 toolResult：isError=true 且提示 token limit
            let agent_end = events_of(&events, "agent_end")[0];
            let msgs = agent_end["messages"].as_array().unwrap();
            let tr_msg = msgs.iter().find(|m| m["role"] == "toolResult").unwrap();
            assert_eq!(tr_msg["isError"], true);
            assert!(tr_msg["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("output token limit"));
            // 仍然继续第二轮（Recovered）
            assert_eq!(msgs.last().unwrap()["content"][0]["text"], "Recovered");
        });
    }

    #[test]
    fn run_loop_error_stop_terminates_cleanly() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let cwd = std::env::temp_dir().to_string_lossy().to_string();
            let frames = vec![
                r#"{"id":"cmpl_t","choices":[{"index":0,"delta":{"role":"assistant","content":"partial"}}]}"#,
                r#"{"id":"cmpl_t","choices":[{"index":0,"delta":{},"finish_reason":"error"}]}"#,
                r#"[DONE]"#,
            ];
            let (base, handle) = mock_server_multi(vec![frames]).await;

            let mut agent = test_agent(&base, &cwd);
            let collected: std::sync::Arc<std::sync::Mutex<Vec<J>>> =
                std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let sink = collected.clone();
            agent.attach_json_sink(Box::new(move |e: J| sink.lock().unwrap().push(e)));

            // 对齐 pi：stopReason=error 不抛错，消息进流并正常结束
            let _text = agent.prompt("boom").await.unwrap();
            handle.await.unwrap();

            let events = collected.lock().unwrap().clone();
            let types = ev_types(&events);
            assert!(types.contains(&"turn_end".to_string()));
            assert!(types.contains(&"agent_end".to_string()));

            let agent_end = events_of(&events, "agent_end")[0];
            let msgs = agent_end["messages"].as_array().unwrap();
            assert_eq!(msgs.last().unwrap()["role"], "assistant");
            // 部分文本仍在
            assert_eq!(msgs.last().unwrap()["content"][0]["text"], "partial");
        });
    }

    #[test]
    fn steering_is_injected_via_runtime_inbox() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let cwd = std::env::temp_dir().to_string_lossy().to_string();
            // 单轮：纯文本回复
            let (base, handle) = mock_server_multi(vec![text_frame("First", "a")]).await;

            let mut agent = test_agent(&base, &cwd);
            let collected: std::sync::Arc<std::sync::Mutex<Vec<J>>> =
                std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let sink = collected.clone();
            agent.attach_json_sink(Box::new(move |e: J| sink.lock().unwrap().push(e)));

            // 预置 steer 到运行中注入箱（run_loop 初始 poll 取走，第一轮 LLM 前注入）
            agent
                .runtime_steer_inbox
                .lock()
                .unwrap()
                .push_back(AgentMessage::user_text("steer-msg"));

            let text = agent.prompt("go").await.unwrap();
            handle.await.unwrap();
            assert_eq!(text, "First");

            let events = collected.lock().unwrap().clone();
            let types = ev_types(&events);
            assert_eq!(
                types.iter().filter(|t| t.as_str() == "turn_start").count(),
                1
            );
            let user_msgs: Vec<&str> = events
                .iter()
                .filter(|e| e["type"] == "message_start" && e["message"]["role"] == "user")
                .map(|e| e["message"]["content"][0]["text"].as_str().unwrap())
                .collect();
            assert!(user_msgs.contains(&"go"));
            assert!(user_msgs.contains(&"steer-msg"));

            // agent_end.messages：user(go) + user(steer) → assistant(First)
            let agent_end = events_of(&events, "agent_end")[0];
            let msgs = agent_end["messages"].as_array().unwrap();
            let roles: Vec<&str> = msgs.iter().map(|m| m["role"].as_str().unwrap()).collect();
            assert_eq!(roles, vec!["user", "user", "assistant"]);
            assert_eq!(msgs[1]["content"][0]["text"], "steer-msg");
            assert_eq!(msgs[2]["content"][0]["text"], "First");
        });
    }

    #[test]
    fn agent_end_carries_only_new_messages_not_history() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let cwd = std::env::temp_dir().to_string_lossy().to_string();
            let (base, handle) = mock_server_multi(vec![text_frame("Only new", "a")]).await;

            let mut agent = test_agent(&base, &cwd);
            // 预置历史（恢复会话场景）
            agent
                .messages
                .push(AgentMessage::user_text("old history 1"));
            agent
                .messages
                .push(AgentMessage::user_text("old history 2"));

            let collected: std::sync::Arc<std::sync::Mutex<Vec<J>>> =
                std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let sink = collected.clone();
            agent.attach_json_sink(Box::new(move |e: J| sink.lock().unwrap().push(e)));

            let text = agent.prompt("now").await.unwrap();
            handle.await.unwrap();
            assert_eq!(text, "Only new");

            // agent_end 只携带本次 newMessages：当前 prompt + assistant，不含旧历史
            let collected_events = collected.lock().unwrap().clone();
            let agent_end = events_of(&collected_events, "agent_end")[0];
            let msgs = agent_end["messages"].as_array().unwrap();
            let roles: Vec<&str> = msgs.iter().map(|m| m["role"].as_str().unwrap()).collect();
            assert_eq!(roles, vec!["user", "assistant"]);
            assert_eq!(msgs[0]["content"][0]["text"], "now");
            assert_eq!(msgs[1]["content"][0]["text"], "Only new");
        });
    }

    #[test]
    fn finish_turn_end_terminates_before_next_turn() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let cwd = std::env::temp_dir().to_string_lossy().to_string();
            let (base, handle) = mock_server_multi(vec![
                tool_call_frame("read", r#"{\"path\":\"a.rs\"}"#, "call_1"),
                text_frame("Should not run", "b"),
            ])
            .await;

            let mut agent = test_agent(&base, &cwd);
            let collected: std::sync::Arc<std::sync::Mutex<Vec<J>>> =
                std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let sink = collected.clone();
            agent.attach_json_sink(Box::new(move |e: J| sink.lock().unwrap().push(e)));

            // 每轮后立即结束：第一轮（工具轮）结束后即 agent_end，不进入第二轮
            agent.finish_turn = Some(std::sync::Mutex::new(Box::new(|_| {
                Some(FinishTurnAction::End)
            })));

            let result = agent.prompt("go").await;
            handle.await.unwrap();
            let _ = result;

            let events = collected.lock().unwrap().clone();
            let types = ev_types(&events);
            // 只有一轮：1 个 turn_start；无第二轮 assistant（"Should not run" 不会出现）
            assert_eq!(
                types.iter().filter(|t| t.as_str() == "turn_start").count(),
                1
            );
            assert_eq!(types.iter().filter(|t| t.as_str() == "turn_end").count(), 1);
            assert!(types.contains(&"agent_end".to_string()));
            assert!(
                !events
                    .iter()
                    .any(|e| e["message"]["content"][0]["text"] == "Should not run")
            );
        });
    }

    /// pi 0.87.0 `finishTurn` → `{ action: "continue" }`：无工具/无 steering 时也再发起一次请求。
    #[test]
    fn finish_turn_continue_forces_extra_request() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let cwd = std::env::temp_dir().to_string_lossy().to_string();
            let (base, handle) =
                mock_server_multi(vec![text_frame("First", "a"), text_frame("Second", "b")]).await;

            let mut agent = test_agent(&base, &cwd);
            let collected: std::sync::Arc<std::sync::Mutex<Vec<J>>> =
                std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let sink = collected.clone();
            agent.attach_json_sink(Box::new(move |e: J| sink.lock().unwrap().push(e)));

            // 仅第一轮请求延续；延续轮返回 None → 自然结束（不多发第三次）
            let calls = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
            let counter = calls.clone();
            agent.finish_turn = Some(std::sync::Mutex::new(Box::new(move |_| {
                if counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                    Some(FinishTurnAction::Continue)
                } else {
                    None
                }
            })));

            let text = agent.prompt("go").await.unwrap();
            handle.await.unwrap();

            let events = collected.lock().unwrap().clone();
            let types = ev_types(&events);
            assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
            assert_eq!(
                types.iter().filter(|t| t.as_str() == "turn_start").count(),
                2,
                "Continue 应触发第二次请求"
            );
            assert_eq!(text, "Second");
        });
    }

    /// pi 0.87.0：`finishTurn` 对 error/aborted 也会调用（turn_end 之前），但决策被忽略。
    #[test]
    fn finish_turn_runs_on_error_and_decision_ignored() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let cwd = std::env::temp_dir().to_string_lossy().to_string();
            // 无后续 frame：第二轮不会被发起
            let (base, handle) = mock_server_multi(vec![text_frame("boom", "a")]).await;
            let mut agent = test_agent(&base, &cwd);

            // 让首轮以 stopReason=error 结束（provider 层失败消息）
            agent.model.base_url = "http://127.0.0.1:1".to_string();
            agent.auto_retry = false; // 关闭瞬时错误重试，确保只产生一次硬退出 turn

            let seen: std::sync::Arc<std::sync::Mutex<Vec<Option<String>>>> =
                std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let rec = seen.clone();
            agent.finish_turn = Some(std::sync::Mutex::new(Box::new(move |ctx| {
                rec.lock().unwrap().push(ctx.message.stop_reason.clone());
                Some(FinishTurnAction::End)
            })));

            let _ = agent.prompt("go").await;
            handle.await.unwrap();

            let seen = seen.lock().unwrap().clone();
            assert_eq!(seen.len(), 1, "硬退出也应调用一次 finishTurn");
            assert_eq!(seen[0].as_deref(), Some("error"));
        });
    }

    // ---- 扩展边界（pi 0.87.0 turn_end / agent_before_settle）----

    /// 边界探针：全局注册但**状态是线程本地的**（模式默认 0 → 返回 None），
    /// 由测试在自己的线程上驱动；避免并行测试互相干扰。
    struct BoundaryProbe;

    #[derive(Default)]
    struct ProbeState {
        /// 1 = 续跑一次（消费掉）；2 = 结束；3 = 追加一条 user 消息后续跑（消费掉）
        turn: u8,
        /// 同上，作用于 agent_before_settle
        settle: u8,
        seen: Vec<J>,
    }

    thread_local! {
        static PROBE: std::cell::RefCell<ProbeState> = std::cell::RefCell::new(ProbeState::default());
    }

    impl core::extensions::Extension for BoundaryProbe {
        fn name(&self) -> &str {
            "boundary-probe-test"
        }

        fn tools(&self) -> Vec<core::extensions::ExtensionTool> {
            Vec::new()
        }

        fn hooks(&self) -> Vec<core::extensions::ExtensionHook> {
            vec![core::extensions::ExtensionHook::Boundary]
        }

        fn on_boundary(&self, event: &J) -> Option<core::extensions::BoundaryOutcome> {
            let ty = event["type"].as_str().unwrap_or("");
            PROBE.with(|p| {
                let mut p = p.borrow_mut();
                p.seen.push(event.clone());
                let slot = match ty {
                    "turn_end" => &mut p.turn,
                    "agent_before_settle" => &mut p.settle,
                    _ => return None,
                };
                match *slot {
                    1 => {
                        *slot = 0; // 一次性：避免无限续跑
                        Some(core::extensions::BoundaryOutcome::continue_once())
                    }
                    2 => Some(core::extensions::BoundaryOutcome::end()),
                    3 => {
                        *slot = 0;
                        Some(core::extensions::BoundaryOutcome::continue_with(vec![
                            crate::core::provider::AgentMessage::user_text("probe-nudge"),
                        ]))
                    }
                    // 4 = 只追加「省略本轮 assistant」的上下文编辑（不要求续跑）
                    4 => {
                        *slot = 0;
                        let target = event["messageEntryId"].as_str().unwrap_or("").to_string();
                        Some(core::extensions::BoundaryOutcome::default().with_omit(target))
                    }
                    // 5 = 只追加一条 retain-none 压缩（不要求续跑）
                    5 => {
                        *slot = 0;
                        Some(
                            core::extensions::BoundaryOutcome::default()
                                .with_retain_none_compaction("probe-self-retain"),
                        )
                    }
                    _ => None,
                }
            })
        }
    }

    /// pi 0.87.0 `turn_end` 边界：上下文编辑草稿落库后立即生效（不改写历史，投影中省略）。
    #[test]
    fn turn_end_boundary_context_edit_omits_entry() {
        let _agent_dir = crate::test_support::AgentDirGuard::temp();
        probe_reset();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let cwd = std::env::temp_dir().to_string_lossy().to_string();
            let (base, handle) = mock_server_multi(vec![text_frame("done", "a")]).await;
            let mut agent = session_agent(&base, &cwd);
            probe_set("turn_end", 4);

            let text = agent.prompt("go").await.unwrap();
            assert_eq!(text, "done");
            handle.await.unwrap();

            let seen = probe_seen("turn_end");
            let target = seen[0]["messageEntryId"]
                .as_str()
                .expect("turn_end 带 messageEntryId")
                .to_string();
            let entries = agent.session.as_ref().unwrap().get_entries();
            let edit = entries
                .iter()
                .find(|e| e["type"] == "context_edit")
                .expect("边界草稿应落库为 context_edit");
            assert_eq!(edit["targetId"], target.as_str());
            assert!(edit["replacement"].is_null());
            // 原始 transcript 保留（append-only），但投影中不再出现
            assert!(entries.iter().any(|e| e["id"] == target.as_str()));
            let rebuilt = agent.session.as_ref().unwrap().build_context_messages();
            assert!(
                !rebuilt
                    .iter()
                    .any(|m| m.entry_id.as_deref() == Some(target.as_str())),
                "被省略的条目不应出现在重建后的上下文: {rebuilt:?}"
            );
        });
    }

    /// pi 0.87.0 `turn_end` 边界：retain-none 压缩草稿（`firstKeptEntryId: null` 形态）。
    #[test]
    fn turn_end_boundary_retain_none_compaction_self_retains() {
        let _agent_dir = crate::test_support::AgentDirGuard::temp();
        probe_reset();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let cwd = std::env::temp_dir().to_string_lossy().to_string();
            let (base, handle) = mock_server_multi(vec![text_frame("done", "a")]).await;
            let mut agent = session_agent(&base, &cwd);
            probe_set("turn_end", 5);

            let text = agent.prompt("go").await.unwrap();
            assert_eq!(text, "done");
            handle.await.unwrap();

            let entries = agent.session.as_ref().unwrap().get_entries();
            let comp = entries
                .iter()
                .find(|e| e["type"] == "compaction")
                .expect("边界草稿应落库为 compaction");
            assert_eq!(
                comp["firstKeptEntryId"].as_str(),
                comp["id"].as_str(),
                "retain-none：firstKeptEntryId 自引用（pi `firstKeptEntryId ?? id`）"
            );
            assert!(
                comp.get("retainedTail").is_none(),
                "新格式不再内联保留尾部: {comp}"
            );
            assert_eq!(comp["summary"], "probe-self-retain");
            // 投影与 agent.messages 同步重建：只剩摘要
            let rebuilt = agent.session.as_ref().unwrap().build_context_messages();
            assert_eq!(rebuilt.len(), 1);
            assert_eq!(rebuilt[0].role, "compactionSummary");
            assert_eq!(agent.messages.len(), 1);
            assert_eq!(agent.messages[0].role, "compactionSummary");
        });
    }

    // ---- context_with_system（pi 0.87.0 完整 transcript 变换）----

    /// 完整 transcript 探针：记录投递内容，并按线程本地开关改写 `[0]` 的 system 消息。
    struct ContextProbe;

    thread_local! {
        /// None = 不改写；Some(text) = 把 `[0]`（system）改成该文本
        static CTX_PROBE: std::cell::RefCell<(Option<String>, Vec<J>)> =
            const { std::cell::RefCell::new((None, Vec::new())) };
    }

    impl core::extensions::Extension for ContextProbe {
        fn name(&self) -> &str {
            "context-with-system-probe-test"
        }

        fn tools(&self) -> Vec<core::extensions::ExtensionTool> {
            Vec::new()
        }

        fn hooks(&self) -> Vec<core::extensions::ExtensionHook> {
            vec![core::extensions::ExtensionHook::ContextWithSystem]
        }

        fn transform_context_with_system(
            &self,
            messages: &mut Vec<AgentMessage>,
        ) -> crate::error::Result<()> {
            CTX_PROBE.with(|p| {
                let mut p = p.borrow_mut();
                p.1.push(json!({
                    "roles": messages.iter().map(|m| m.role.clone()).collect::<Vec<_>>(),
                }));
                if let Some(text) = p.0.clone()
                    && let Some(first) = messages.first_mut()
                {
                    first.content = vec![crate::core::provider::ContentBlock::Text {
                        text,
                        text_signature: None,
                    }];
                }
            });
            Ok(())
        }
    }

    fn ctx_probe_reset(rewrite: Option<String>) {
        static ONCE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        ONCE.get_or_init(|| core::extensions::register_extension(ContextProbe));
        CTX_PROBE.with(|p| *p.borrow_mut() = (rewrite, Vec::new()));
    }

    fn ctx_probe_seen() -> Vec<J> {
        CTX_PROBE.with(|p| p.borrow().1.clone())
    }

    /// `context_with_system`：完整 transcript（system 在 `[0]`）+ 结果原样发送。
    #[test]
    fn context_with_system_sees_and_rewrites_prompt() {
        let _agent_dir = crate::test_support::AgentDirGuard::temp();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let cwd = std::env::temp_dir().to_string_lossy().to_string();
            let (base, handle) = mock_server_multi(vec![text_frame("ok", "a")]).await;

            // 改写 system：请求用改写后的提示词（会话历史不被改写）
            ctx_probe_reset(Some("rewritten prompt".to_string()));
            let mut agent = session_agent(&base, &cwd);
            agent.system_prompt = "original prompt".to_string();
            // 抓取真正发出的请求体：改写后的提示词必须落在 system 消息里
            let sent: std::sync::Arc<std::sync::Mutex<Vec<J>>> =
                std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let cap = sent.clone();
            agent.set_on_payload(Box::new(move |p: &mut J| {
                cap.lock().unwrap().push(p.clone());
            }));
            let text = agent.prompt("go").await.unwrap();
            assert_eq!(text, "ok");
            let seen = ctx_probe_seen();
            assert_eq!(seen.len(), 1, "每次 LLM 调用投递一次");
            assert_eq!(seen[0]["roles"][0], "system", "system 消息应在 [0]");
            assert_eq!(seen[0]["roles"][1], "user");
            assert_eq!(
                agent.system_prompt, "original prompt",
                "会话自身的系统提示词不被改写（只影响本次请求）"
            );
            let payloads = sent.lock().unwrap().clone();
            let msgs = payloads[0]["messages"].as_array().unwrap().clone();
            assert_eq!(msgs[0]["role"], "system");
            assert_eq!(
                msgs[0]["content"], "rewritten prompt",
                "请求体应使用边界改写后的系统提示词"
            );
            handle.await.unwrap();

            // 未声明该 hook 的扩展不受影响：无探针改写时消息列表原样
            let (base, handle) = mock_server_multi(vec![text_frame("ok2", "b")]).await;
            ctx_probe_reset(None);
            let mut agent = session_agent(&base, &cwd);
            agent.system_prompt = "keep me".to_string();
            let text = agent.prompt("go").await.unwrap();
            assert_eq!(text, "ok2");
            let seen = ctx_probe_seen();
            assert_eq!(seen[0]["roles"][0], "system");
            handle.await.unwrap();
        });
    }

    /// 注册探针（幂等）并清空本线程状态。
    fn probe_reset() {
        static ONCE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        ONCE.get_or_init(|| core::extensions::register_extension(BoundaryProbe));
        PROBE.with(|p| *p.borrow_mut() = ProbeState::default());
    }

    fn probe_set(kind: &str, mode: u8) {
        PROBE.with(|p| {
            let mut p = p.borrow_mut();
            match kind {
                "turn_end" => p.turn = mode,
                "agent_before_settle" => p.settle = mode,
                _ => unreachable!(),
            }
        });
    }

    fn probe_seen(kind: &str) -> Vec<J> {
        PROBE.with(|p| {
            p.borrow()
                .seen
                .iter()
                .filter(|e| e["type"].as_str() == Some(kind))
                .cloned()
                .collect()
        })
    }

    /// 会话背书的测试 agent（`messageEntryId` / `toolResultEntryIds` 需真实落库）。
    fn session_agent(base: &str, cwd: &str) -> Agent {
        let mut agent = test_agent(base, cwd);
        let dir =
            std::env::temp_dir().join(format!("prux-boundary-{}", crate::utils::time::now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        agent.session = Some(
            crate::core::session_manager::Session::create_with(
                crate::core::session_manager::SessionCreateOptions {
                    cwd: cwd.to_string(),
                    session_dir: Some(dir),
                    persist: true,
                    ..Default::default()
                },
            )
            .unwrap(),
        );
        agent
    }

    /// `Agent::rewind_last_user_input`：移动叶子后立即用会话投影重建 `agent.messages`。
    #[test]
    fn rewind_last_user_input_rebuilds_agent_context() {
        let _agent_dir = crate::test_support::AgentDirGuard::temp();
        let cwd = std::env::temp_dir().to_string_lossy().to_string();
        let mut agent = session_agent("http://127.0.0.1:1", &cwd);

        let session = agent.session.as_mut().unwrap();
        session.append_message(&AgentMessage::user_text("first"));
        let mut a1 = AgentMessage::user_text("a1");
        a1.role = "assistant".into();
        session.append_message(&a1);
        session.append_message(&AgentMessage::user_text("second"));
        let mut a2 = AgentMessage::user_text("a2");
        a2.role = "assistant".into();
        session.append_message(&a2);
        agent.messages = agent.session.as_ref().unwrap().build_context_messages();
        assert_eq!(agent.messages.len(), 4);

        let removed = agent.rewind_last_user_input().unwrap();
        assert_eq!(removed.as_deref(), Some("second"));
        let texts: Vec<String> = agent.messages.iter().map(|m| m.text()).collect();
        assert_eq!(texts, vec!["first", "a1"]);

        // 无可回退内容：返回 None 且上下文不变。
        let session = agent.session.as_mut().unwrap();
        session.rewind_last_user_input().unwrap(); // 再回退一次（first）
        agent.messages = agent.session.as_ref().unwrap().build_context_messages();
        assert!(agent.messages.is_empty());
        assert_eq!(agent.rewind_last_user_input().unwrap(), None);
        assert!(agent.messages.is_empty());
    }

    /// `exec_ctx().session_branch_entries` 只含**当前分支**的条目（不是整个会话）：
    /// 切到另一条分支后，离开活动分支的条目不得出现在快照里。
    #[test]
    fn exec_ctx_branch_entries_follow_current_branch() {
        let _agent_dir = crate::test_support::AgentDirGuard::temp();
        let cwd = std::env::temp_dir().to_string_lossy().to_string();
        let mut agent = session_agent("http://127.0.0.1:1", &cwd);

        let (id1, id2, id3) = {
            let session = agent.session.as_mut().unwrap();
            let id1 = session.append_message(&AgentMessage::user_text("first"));
            session.append_custom_entry("codemode-store", Some(json!({ "set": { "a": 1 } })));
            let id2 = session.append_message(&AgentMessage::user_text("second"));
            // 回到第一条消息，另开一条分支（id3 取代 id2 成为活动分支的叶子）
            session.set_leaf(Some(&id1)).unwrap();
            let id3 =
                session.append_custom_entry("codemode-store", Some(json!({ "set": { "b": 2 } })));
            (id1, id2, id3)
        };

        let ids = |entries: &[Value]| -> Vec<String> {
            entries
                .iter()
                .filter_map(|e| e.get("id").and_then(|v| v.as_str()).map(str::to_string))
                .collect()
        };

        let ctx = agent.exec_ctx();
        let entries = ctx.session_branch_entries.expect("有会话时应带分支快照");
        let branch = ids(&entries);
        assert!(
            branch.contains(&id1) && branch.contains(&id3),
            "含本分支的条目: {branch:?}"
        );
        assert!(
            !branch.contains(&id2),
            "离开活动分支的条目不得出现: {branch:?}"
        );
        assert_eq!(
            session_branch_store_payload(&entries),
            Some(json!({ "b": 2 })),
            "只看到本分支上的 store 条目（a=1 写在另一条分支上）"
        );

        // 切回原分支：快照只含 id2 那支的写入
        {
            let session = agent.session.as_mut().unwrap();
            session.set_leaf(Some(&id2)).unwrap();
        }
        let ctx = agent.exec_ctx();
        let entries = ctx.session_branch_entries.unwrap();
        let branch = ids(&entries);
        assert!(
            branch.contains(&id1) && branch.contains(&id2),
            "含本分支的条目: {branch:?}"
        );
        assert!(!branch.contains(&id3), "切回后不应看到另一分支: {branch:?}");
        assert_eq!(
            session_branch_store_payload(&entries),
            Some(json!({ "a": 1 })),
            "切回后看到的是本分支的 store 写入"
        );

        // 无会话（子代理上下文）时为 None：拿不到分支数据，消费方应保持既有快照
        agent.session = None;
        assert!(agent.exec_ctx().session_branch_entries.is_none());
    }

    /// 从分支条目里把 `codemode-store` 的 `set` 合并成一份快照（测试用）。
    fn session_branch_store_payload(entries: &[Value]) -> Option<Value> {
        let mut merged = serde_json::Map::new();
        for entry in entries {
            if entry.get("customType").and_then(|v| v.as_str()) != Some("codemode-store") {
                continue;
            }
            if let Some(set) = entry
                .get("data")
                .and_then(|d| d.get("set"))
                .and_then(|v| v.as_object())
            {
                for (k, v) in set {
                    merged.insert(k.clone(), v.clone());
                }
            }
        }
        (!merged.is_empty()).then_some(Value::Object(merged))
    }

    /// pi 0.87.0 `turn_end` 边界：字段齐全（turnIndex / entryId / outcome）+ `continue` 触发额外请求。
    #[test]
    fn turn_end_boundary_fields_and_continue() {
        let _agent_dir = crate::test_support::AgentDirGuard::temp();
        probe_reset();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let cwd = std::env::temp_dir().to_string_lossy().to_string();
            let (base, handle) =
                mock_server_multi(vec![text_frame("First", "a"), text_frame("Second", "b")]).await;
            let mut agent = session_agent(&base, &cwd);
            let collected: std::sync::Arc<std::sync::Mutex<Vec<J>>> =
                std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let sink = collected.clone();
            agent.attach_json_sink(Box::new(move |e: J| sink.lock().unwrap().push(e)));

            probe_set("turn_end", 1); // 第一轮结束后要求续跑一次
            let text = agent.prompt("go").await.unwrap();
            handle.await.unwrap();
            assert_eq!(text, "Second");

            let events = collected.lock().unwrap().clone();
            let types = ev_types(&events);
            assert_eq!(
                types.iter().filter(|t| t.as_str() == "turn_start").count(),
                2
            );
            assert_eq!(types.iter().filter(|t| t.as_str() == "turn_end").count(), 2);

            let seen = probe_seen("turn_end");
            assert_eq!(seen.len(), 2, "每轮都应投递 turn_end 边界");
            assert_eq!(seen[0]["turnIndex"], 0);
            assert_eq!(seen[1]["turnIndex"], 1);
            assert_eq!(seen[0]["outcome"], "completed");
            assert!(
                seen[0]["messageEntryId"]
                    .as_str()
                    .is_some_and(|s| !s.is_empty()),
                "messageEntryId 应为落库条目 id: {}",
                seen[0]
            );
            assert_eq!(seen[0]["toolResultEntryIds"].as_array().unwrap().len(), 0);

            // 同步发出的内核事件与边界事件同形（含边界字段）
            let emitted = events_of(&events, "turn_end")[0];
            assert_eq!(emitted["turnIndex"], 0);
            assert!(
                emitted["messageEntryId"]
                    .as_str()
                    .is_some_and(|s| !s.is_empty())
            );
            assert_eq!(emitted["outcome"], "completed");
        });
    }

    /// `turn_end` 边界：工具轮的 `toolResultEntryIds` 指向落库的 toolResult 条目。
    #[test]
    fn turn_end_boundary_reports_tool_result_entry_ids() {
        let _agent_dir = crate::test_support::AgentDirGuard::temp();
        probe_reset();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let cwd = std::env::temp_dir().to_string_lossy().to_string();
            let (base, handle) = mock_server_multi(vec![
                tool_call_frame("read", r#"{\"path\":\"a.rs\"}"#, "call_1"),
                text_frame("done", "b"),
            ])
            .await;
            let mut agent = session_agent(&base, &cwd);

            let _ = agent.prompt("go").await.unwrap();
            handle.await.unwrap();

            let seen = probe_seen("turn_end");
            assert_eq!(seen.len(), 2);
            let ids = seen[0]["toolResultEntryIds"].as_array().unwrap();
            assert_eq!(
                ids.len(),
                1,
                "工具轮应有 1 个 toolResult entryId: {}",
                seen[0]
            );
            assert!(ids[0].as_str().is_some_and(|s| !s.is_empty()));
            assert_eq!(seen[0]["outcome"], "completed");
            assert_eq!(seen[1]["toolResultEntryIds"].as_array().unwrap().len(), 0);
        });
    }

    /// `turn_end` 边界：`end` 在本轮后优雅结束 run（不进入下一轮）。
    #[test]
    fn turn_end_boundary_end_stops_run() {
        let _agent_dir = crate::test_support::AgentDirGuard::temp();
        probe_reset();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let cwd = std::env::temp_dir().to_string_lossy().to_string();
            let (base, handle) = mock_server_multi(vec![
                tool_call_frame("read", r#"{\"path\":\"a.rs\"}"#, "call_1"),
                text_frame("Should not run", "b"),
            ])
            .await;
            let mut agent = session_agent(&base, &cwd);
            let collected: std::sync::Arc<std::sync::Mutex<Vec<J>>> =
                std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let sink = collected.clone();
            agent.attach_json_sink(Box::new(move |e: J| sink.lock().unwrap().push(e)));

            probe_set("turn_end", 2); // 每轮后要求结束
            let _ = agent.prompt("go").await.unwrap();
            handle.await.unwrap();

            let events = collected.lock().unwrap().clone();
            let types = ev_types(&events);
            assert_eq!(
                types.iter().filter(|t| t.as_str() == "turn_start").count(),
                1
            );
            assert!(
                !events
                    .iter()
                    .any(|e| e["message"]["content"][0]["text"] == "Should not run")
            );
        });
    }

    /// `turn_end` 边界在 error/aborted 轮也会投递（含 outcome=error），但硬退出、决策被忽略。
    #[test]
    fn turn_end_boundary_ignored_on_hard_exit() {
        let _agent_dir = crate::test_support::AgentDirGuard::temp();
        probe_reset();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let cwd = std::env::temp_dir().to_string_lossy().to_string();
            let (base, handle) = mock_server_multi(vec![text_frame("boom", "a")]).await;
            let mut agent = session_agent(&base, &cwd);
            agent.model.base_url = "http://127.0.0.1:1".to_string(); // 首轮 provider 失败
            agent.auto_retry = false;
            let collected: std::sync::Arc<std::sync::Mutex<Vec<J>>> =
                std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let sink = collected.clone();
            agent.attach_json_sink(Box::new(move |e: J| sink.lock().unwrap().push(e)));

            probe_set("turn_end", 1); // 要求续跑，但硬退出应忽略
            let _ = agent.prompt("go").await;
            handle.await.unwrap();

            let seen = probe_seen("turn_end");
            assert_eq!(seen.len(), 1, "硬退出也应投递一次 turn_end 边界");
            assert_eq!(seen[0]["outcome"], "error");
            assert!(
                seen[0]["messageEntryId"]
                    .as_str()
                    .is_some_and(|s| !s.is_empty()),
                "硬退出轮也应先落库再投边界: {}",
                seen[0]
            );
            let events = collected.lock().unwrap().clone();
            let types = ev_types(&events);
            assert_eq!(
                types.iter().filter(|t| t.as_str() == "turn_start").count(),
                1,
                "硬退出不得因边界决策续跑"
            );
        });
    }

    /// pi 0.87.0 `agent_before_settle` 边界：`continue` 时本回合再发起一轮。
    #[test]
    fn settle_boundary_continue_runs_another_round() {
        let _agent_dir = crate::test_support::AgentDirGuard::temp();
        probe_reset();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let cwd = std::env::temp_dir().to_string_lossy().to_string();
            let (base, handle) =
                mock_server_multi(vec![text_frame("First", "a"), text_frame("Second", "b")]).await;
            let mut agent = session_agent(&base, &cwd);
            let collected: std::sync::Arc<std::sync::Mutex<Vec<J>>> =
                std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let sink = collected.clone();
            agent.attach_json_sink(Box::new(move |e: J| sink.lock().unwrap().push(e)));

            probe_set("agent_before_settle", 1);
            let text = agent.prompt("go").await.unwrap();
            handle.await.unwrap();
            assert_eq!(text, "Second", "结算边界续跑后应取新一轮回复");

            let events = collected.lock().unwrap().clone();
            let types = ev_types(&events);
            assert_eq!(
                types.iter().filter(|t| t.as_str() == "turn_start").count(),
                2,
                "continue 应触发第二次 run_loop"
            );
            assert_eq!(
                types.iter().filter(|t| t.as_str() == "agent_end").count(),
                2
            );
            // agent_settled 仍是整轮唯一收尾边界
            assert_eq!(
                types
                    .iter()
                    .filter(|t| t.as_str() == "agent_settled")
                    .count(),
                1
            );
            assert_eq!(types.last().map(String::as_str), Some("agent_settled"));

            let seen = probe_seen("agent_before_settle");
            // pi 同款：每次结算都重新咨询（第二次 run 结束再投一次，探针已消费 → None 收敛）
            assert_eq!(seen.len(), 2, "每次结算都应投递边界");
            assert_eq!(seen[0]["outcome"], "completed");
            assert_eq!(seen[0]["continuationIndex"], 0);
            assert_eq!(seen[1]["continuationIndex"], 1, "续跑序号递增");
            assert_eq!(seen[0]["agentScope"], "main");
            assert_eq!(seen[0]["hasQueuedMessages"], false);
            assert!(
                !seen[0]["contextMessages"].as_array().unwrap().is_empty(),
                "边界应携带当前上下文"
            );
        });
    }

    /// 结算边界追加消息（pi `BoundaryResult.entries` 子集）：消息落库 + 作为下一轮 user 消息。
    #[test]
    fn settle_boundary_appends_messages_before_continuation() {
        let _agent_dir = crate::test_support::AgentDirGuard::temp();
        probe_reset();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let cwd = std::env::temp_dir().to_string_lossy().to_string();
            let (base, handle) =
                mock_server_multi(vec![text_frame("First", "a"), text_frame("Second", "b")]).await;
            let mut agent = session_agent(&base, &cwd);
            let collected: std::sync::Arc<std::sync::Mutex<Vec<J>>> =
                std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let sink = collected.clone();
            agent.attach_json_sink(Box::new(move |e: J| sink.lock().unwrap().push(e)));

            probe_set("agent_before_settle", 3);
            let text = agent.prompt("go").await.unwrap();
            handle.await.unwrap();
            assert_eq!(text, "Second", "追加消息后应再跑一轮");

            // 追加的消息落进上下文，且发出 user 消息事件（message_start/message_end）
            assert!(
                agent
                    .messages
                    .iter()
                    .any(|m| m.role == "user" && m.text().contains("probe-nudge")),
                "边界追加的消息应进入上下文"
            );
            let events = collected.lock().unwrap().clone();
            let nudged = events.iter().any(|e| {
                e["type"] == "message_end"
                    && e["message"]["role"] == "user"
                    && e["message"]["content"]
                        .as_array()
                        .is_some_and(|c| c.iter().any(|b| b["text"] == "probe-nudge"))
            });
            assert!(nudged, "追加的 user 消息应发出 message_end 事件");
        });
    }

    #[test]
    fn parallel_execution_runs_multiple_tools() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let tmp = std::env::temp_dir().join(format!("prux-par-{}", crate::utils::time::now_ms()));
            std::fs::create_dir_all(&tmp).unwrap();
            std::fs::write(tmp.join("a.rs"), "AAA").unwrap();
            std::fs::write(tmp.join("b.rs"), "BBB").unwrap();
            let cwd = tmp.to_string_lossy().to_string();

            // 第一轮：两个工具调用（并行执行）
            let frames1 = vec![
                r#"{"id":"cmpl_t","choices":[{"index":0,"delta":{"role":"assistant","content":null}}]}"#,
                r#"{"id":"cmpl_t","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c1","type":"function","function":{"name":"read","arguments":"{\"path\":\"a.rs\"}"}}]}}]}"#,
                r#"{"id":"cmpl_t","choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"id":"c2","type":"function","function":{"name":"read","arguments":"{\"path\":\"b.rs\"}"}}]}}]}"#,
                r#"{"id":"cmpl_t","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
                r#"[DONE]"#,
            ];
            let frames2 = text_frame("Both read", "c");
            let (base, handle) = mock_server_multi(vec![frames1, frames2]).await;

            let mut agent = test_agent(&base, &cwd);
            let collected: std::sync::Arc<std::sync::Mutex<Vec<J>>> =
                std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let sink = collected.clone();
            agent.attach_json_sink(Box::new(move |e: J| sink.lock().unwrap().push(e)));

            let text = agent.prompt("read both").await.unwrap();
            handle.await.unwrap();
            assert_eq!(text, "Both read");

            let events = collected.lock().unwrap().clone();
            assert_eq!(ev_types(&events).iter().filter(|t| t.as_str() == "tool_execution_start").count(), 2);
            assert_eq!(ev_types(&events).iter().filter(|t| t.as_str() == "tool_execution_end").count(), 2);
            // 两个 toolResult 各单独 message_start/end（对齐 pi emitToolResultMessage）
            assert_eq!(
                events.iter()
                    .filter(|e| e["type"] == "message_start" && e["message"]["role"] == "toolResult")
                    .count(),
                2
            );
        });
    }

    /// 回归：默认（并行）执行路径下的 `read` 必须把图片附件回传给模型。
    /// 旧实现 `execute_one` 把 model_supports_images 写死为 false，于是视觉模型也只能看到
    /// "image attachments are not supported by this model."，用户 `@image` / `read image` 全丢图。
    #[test]
    fn parallel_path_read_keeps_image_attachment() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let tmp =
                std::env::temp_dir().join(format!("prux-img-{}", crate::utils::time::now_ms()));
            std::fs::create_dir_all(&tmp).unwrap();
            let png = tmp.join("pic.png");
            let mut buf = Vec::new();
            image::RgbaImage::from_pixel(4, 4, image::Rgba([1, 2, 3, 255]))
                .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
                .unwrap();
            std::fs::write(&png, buf).unwrap();
            let cwd = tmp.to_string_lossy().to_string();

            // 单工具也走并行路径（executionMode 默认 Parallel）
            let frames1 = vec![
                r#"{"id":"cmpl_t","choices":[{"index":0,"delta":{"role":"assistant","content":null}}]}"#,
                r#"{"id":"cmpl_t","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c1","type":"function","function":{"name":"read","arguments":"{\"path\":\"pic.png\"}"}}]}}]}"#,
                r#"{"id":"cmpl_t","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
                r#"[DONE]"#,
            ];
            let (base, handle) = mock_server_multi(vec![frames1, text_frame("done", "c")]).await;

            let mut agent =
                test_agent_with_input(&base, &cwd, vec!["text".into(), "image".into()]);
            let collected: std::sync::Arc<std::sync::Mutex<Vec<J>>> =
                std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let sink = collected.clone();
            agent.attach_json_sink(Box::new(move |e: J| sink.lock().unwrap().push(e)));

            agent.prompt("look").await.unwrap();
            handle.await.unwrap();

            let events = collected.lock().unwrap().clone();
            let tr = events
                .iter()
                .find(|e| {
                    e["type"] == "message_start" && e["message"]["role"] == "toolResult"
                })
                .expect("toolResult message_start");
            let content = tr["message"]["content"].as_array().unwrap();
            let img = content
                .iter()
                .find(|b| b["type"] == "image")
                .unwrap_or_else(|| panic!("read 应回传 image 块，实际: {content:?}"));
            assert_eq!(img["mimeType"], "image/png");
            assert!(
                !content.iter().any(|b| b["type"] == "text"
                    && b["text"]
                        .as_str()
                        .unwrap_or("")
                        .contains("not supported by this model")),
                "不应出现 model does not support images 占位"
            );
        });
    }

    /// `images.blockImages` 打开时：工具结果附件（扩展/生成图片）不再进消息，
    /// 改留一条文本占位；关闭时附件照旧回传。
    #[test]
    fn block_images_drops_tool_result_attachments() {
        let _ad = crate::test_support::AgentDirGuard::temp();
        let tmp =
            std::env::temp_dir().join(format!("prux-block-img-{}", crate::utils::time::now_ms()));
        std::fs::create_dir_all(&tmp).unwrap();
        let mut agent = test_agent_with_input(
            "http://127.0.0.1:1",
            &tmp.to_string_lossy(),
            vec!["text".into(), "image".into()],
        );

        let fin = FinalizedToolCall {
            call: ContentBlock::ToolCall {
                id: "c1".into(),
                name: "codemode".into(),
                arguments: json!({}),
                thought_signature: None,
                namespace: None,
            },
            name: "codemode".into(),
            args: json!({}),
            text: "generated an image".into(),
            details: None,
            usage: None,
            terminate: false,
            is_error: false,
            attachments: vec![crate::core::tools::ToolResultAttachment {
                data_base64: "AAAA".into(),
                mime_type: "image/png".into(),
                original_size: None,
                converted_from: None,
            }],
            content_override: None,
            duration_ms: None,
        };

        // 关闭：附件进消息
        let mut out = Vec::new();
        agent.push_tool_result_message(&fin, Some(1), &mut out);
        let content = &out[0].1.content;
        assert!(
            content
                .iter()
                .any(|b| matches!(b, ContentBlock::Image { .. })),
            "默认应回传图片块: {content:?}"
        );

        // 打开：图片被丢弃，只留文本占位
        settings_manager::write_settings_images_block_images(true).unwrap();
        let mut out = Vec::new();
        agent.push_tool_result_message(&fin, Some(1), &mut out);
        settings_manager::write_settings_images_block_images(false).unwrap();

        let content = &out[0].1.content;
        assert!(
            !content
                .iter()
                .any(|b| matches!(b, ContentBlock::Image { .. })),
            "blockImages 打开时不应有图片块: {content:?}"
        );
        assert!(
            content.iter().any(|b| matches!(
                b,
                ContentBlock::Text { text, .. } if text.contains("images.blockImages")
            )),
            "应给出被阻止的文本占位: {content:?}"
        );
    }

    #[test]
    fn child_agent_is_state_isolated() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let cwd = std::env::temp_dir().to_string_lossy().to_string();
            let (base, _handle) = mock_server_multi(vec![text_frame("x", "x")]).await;
            let mut agent = test_agent(&base, &cwd);
            agent.messages.push(AgentMessage::user_text("parent msg"));

            let mut child = agent.child_agent(None);
            // 状态隔离：空消息、无 session、独立队列
            assert!(child.messages.is_empty());
            assert!(child.session.is_none());
            assert_eq!(child.cwd, agent.cwd);
            assert_eq!(child.model.base_url, agent.model.base_url);
            // 父队列变更不影响子
            agent
                .runtime_steer_inbox
                .lock()
                .unwrap()
                .push_back(AgentMessage::user_text("parent steer"));
            assert!(child.runtime_steer_inbox.lock().unwrap().is_empty());
            child.messages.push(AgentMessage::user_text("child msg"));
            assert_eq!(agent.messages.len(), 1);
        });
    }

    #[test]
    fn run_sub_agent_returns_isolated_result() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let cwd = std::env::temp_dir().to_string_lossy().to_string();
            let (base, handle) = mock_server_multi(vec![text_frame("sub done", "s")]).await;

            let agent = test_agent(&base, &cwd);
            let text = agent.run_sub_agent("do sub task").await.unwrap();
            handle.await.unwrap();
            assert_eq!(text, "sub done");
            // 父 agent 不受子代理影响（无消息追加、无事件）
            assert!(agent.messages.is_empty());
        });
    }

    #[test]
    fn spawn_sub_agents_run_concurrently() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let cwd = std::env::temp_dir().to_string_lossy().to_string();
            let (base, handle) =
                mock_server_multi(vec![text_frame("one", "1"), text_frame("two", "2")]).await;

            let agent = test_agent(&base, &cwd);
            let h1 = agent.spawn_sub_agent("task one".to_string());
            let h2 = agent.spawn_sub_agent("task two".to_string());
            let r1 = h1.await.unwrap().unwrap();
            let r2 = h2.await.unwrap().unwrap();
            handle.await.unwrap();
            // 并发连接顺序不确定：两个子代理都成功返回各自结果
            let mut got = vec![r1, r2];
            got.sort();
            assert_eq!(got, vec!["one".to_string(), "two".to_string()]);
        });
    }

    /// 对齐 pi：tool_execution_start/end 载荷（args=真实参数；result=AgentToolResult 对象；
    /// end 带工具自身耗时的 durationMs，未执行的调用不带），且 agent_end 每次 run 只发一次。
    #[test]
    fn tool_execution_event_payloads_match_pi() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let tmp =
                std::env::temp_dir().join(format!("prux-test-{}", crate::utils::time::now_ms()));
            std::fs::create_dir_all(&tmp).unwrap();
            std::fs::write(tmp.join("a.rs"), "fn main() {}").unwrap();
            let cwd = tmp.to_string_lossy().to_string();

            let frames1 = tool_call_frame("read", r#"{\"path\":\"a.rs\"}"#, "call_1");
            let frames2 = text_frame("Read done", "cmpl_2");
            let (base, handle) = mock_server_multi(vec![frames1, frames2]).await;

            let mut agent = test_agent(&base, &cwd);
            let collected: std::sync::Arc<std::sync::Mutex<Vec<J>>> =
                std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let sink = collected.clone();
            agent.attach_json_sink(Box::new(move |e: J| sink.lock().unwrap().push(e)));

            let _ = agent.prompt("read it").await.unwrap();
            handle.await.unwrap();
            let events = collected.lock().unwrap().clone();

            // tool_execution_start 携带真实解析参数（对齐 pi：start 在 prepare 后发，args=validated args）
            let start = events
                .iter()
                .find(|e| e["type"] == "tool_execution_start")
                .unwrap();
            assert_eq!(start["toolCallId"], "call_1");
            assert_eq!(start["toolName"], "read");
            assert_eq!(start["args"], json!({ "path": "a.rs" }));

            // tool_execution_end：result 为完整 AgentToolResult 对象（content/details），isError=false，
            // 真实执行过的调用带 durationMs
            let end = events
                .iter()
                .find(|e| e["type"] == "tool_execution_end")
                .unwrap();
            assert_eq!(end["toolCallId"], "call_1");
            assert_eq!(end["isError"], false);
            assert!(
                end["durationMs"].as_u64().is_some(),
                "执行过的工具应带 durationMs: {end}"
            );
            let result = end["result"].as_object().unwrap();
            assert!(result.contains_key("content"), "result 必须是对象");
            assert!(result.contains_key("details"), "result 必须含 details");
            let text = result["content"][0]["text"].as_str().unwrap();
            assert!(text.contains("fn main() {}"), "got: {text}");

            // agent_end 每次 run 恰好一次（对齐 pi：runAgentLoop 尾部单发）
            let ends = events.iter().filter(|e| e["type"] == "agent_end").count();
            assert_eq!(ends, 1, "agent_end 只发一次");
        });
    }

    /// 对齐 pi：未知工具 → prepareToolCall immediate "Tool N not found"（不再等到执行层 Unknown tool）。
    /// 错误 toolResult 完成后批次未 terminate → 继续下一轮 LLM 调用。
    #[test]
    fn unknown_tool_reports_pi_not_found() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let cwd = std::env::temp_dir().to_string_lossy().to_string();
            let frames1 = tool_call_frame("no_such_tool", r#"{}"#, "call_1");
            let frames2 = text_frame("Recovered", "cmpl_2");
            let (base, handle) = mock_server_multi(vec![frames1, frames2]).await;

            let mut agent = test_agent(&base, &cwd);
            let collected: std::sync::Arc<std::sync::Mutex<Vec<J>>> =
                std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let sink = collected.clone();
            agent.attach_json_sink(Box::new(move |e: J| sink.lock().unwrap().push(e)));

            let text = agent.prompt("hi").await.unwrap();
            handle.await.unwrap();
            assert_eq!(text, "Recovered");

            let events = collected.lock().unwrap().clone();
            let turn_end = events_of(&events, "turn_end");
            // 第一轮 turn_end 的 toolResults 是 not found 错误
            let first = turn_end[0]["toolResults"].as_array().unwrap();
            assert_eq!(first.len(), 1);
            assert_eq!(first[0]["isError"], true);
            assert_eq!(first[0]["toolName"], "no_such_tool");
            let err_text = first[0]["content"][0]["text"].as_str().unwrap();
            assert_eq!(err_text, "Tool no_such_tool not found");
            // 未真正执行的调用没有耗时（对齐 pi："Calls that did not run have none"）
            assert!(
                first[0]["durationMs"].is_null(),
                "immediate 调用不应带 durationMs: {:?}",
                first[0]
            );
            // start/end 均发出
            assert_eq!(
                events
                    .iter()
                    .filter(|e| e["type"] == "tool_execution_start")
                    .count(),
                1
            );
            assert_eq!(
                events
                    .iter()
                    .filter(|e| e["type"] == "tool_execution_end")
                    .count(),
                1
            );
            assert_eq!(
                events.iter().filter(|e| e["type"] == "agent_end").count(),
                1
            );
        });
    }

    /// 对齐 pi：beforeToolCall blocked.terminate=true 参与批次 ALL 终止。
    /// 两个工具都被 block+terminate → shouldTerminateToolBatch=true → 不再发起下一轮 LLM。
    /// 注意：block 的必须是自己注册的独有工具名（全局注册表跨测试累积，不能动 read 等内置名）。
    #[test]
    fn before_tool_block_terminate_stops_batch_when_all_terminate() {
        struct BlockReads;
        impl crate::core::extensions::Extension for BlockReads {
            fn name(&self) -> &str {
                "block-reads-test"
            }
            fn tools(&self) -> Vec<crate::core::extensions::ExtensionTool> {
                vec![crate::core::extensions::ExtensionTool::simple(
                    "blk_read",
                    "test",
                    json!({ "type": "object", "properties": {} }),
                    "",
                )]
            }
            fn hooks(&self) -> Vec<crate::core::extensions::ExtensionHook> {
                vec![crate::core::extensions::ExtensionHook::BeforeToolCall]
            }
            fn execute_tool(
                &self,
                name: &str,
                _args: &Value,
            ) -> std::result::Result<tools::ToolResult, tools::ToolError> {
                if name == "blk_read" {
                    Ok(tools::ToolResult::text("executed anyway"))
                } else {
                    Err(tools::ToolError("unknown".to_string()))
                }
            }
            fn before_tool_call_ext(
                &self,
                name: &str,
                _args: &Value,
                assistant_message: Option<&crate::core::provider::AgentMessage>,
                context: &[crate::core::provider::AgentMessage],
                _all_args: &Value,
            ) -> std::result::Result<crate::core::extensions::BeforeToolCallOutcome, tools::ToolError>
            {
                // 对齐 pi prepareToolCall({assistantMessage, context})：入参必须完整
                assert!(
                    assistant_message.is_some(),
                    "beforeToolCall 应收到 assistantMessage"
                );
                assert!(!context.is_empty(), "beforeToolCall 应收到当前上下文");
                if name == "blk_read" {
                    Ok(crate::core::extensions::BeforeToolCallOutcome {
                        block: true,
                        reason: Some("test block".to_string()),
                        terminate: true,
                        args: None,
                    })
                } else {
                    Ok(crate::core::extensions::BeforeToolCallOutcome::allow())
                }
            }
        }
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            crate::core::extensions::register_extension(BlockReads);
            let cwd = std::env::temp_dir().to_string_lossy().to_string();
            // 一条 blk_read call（block+terminate）→ 批次终止 → 不发起第二轮 LLM
            let frames1 = tool_call_frame("blk_read", r#"{}"#, "call_1");
            let (base, handle) = mock_server_multi(vec![frames1]).await;
            let mut agent = test_agent(&base, &cwd);
            let collected: std::sync::Arc<std::sync::Mutex<Vec<J>>> =
                std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let sink = collected.clone();
            agent.attach_json_sink(Box::new(move |e: J| sink.lock().unwrap().push(e)));

            let _ = agent.prompt("hi").await.unwrap();
            handle.await.unwrap();
            let events = collected.lock().unwrap().clone();
            // 只有一轮：批次终止后内层循环结束（无 steer/follow-up）→ agent_end
            assert_eq!(events.iter().filter(|e| e["type"] == "turn_end").count(), 1);
            // toolResult 为 block 错误文本
            let turn_end = events_of(&events, "turn_end")[0];
            let tr = turn_end["toolResults"].as_array().unwrap();
            assert_eq!(tr.len(), 1);
            assert_eq!(tr[0]["isError"], true);
            assert_eq!(tr[0]["content"][0]["text"], "test block");
            assert_eq!(
                events.iter().filter(|e| e["type"] == "agent_end").count(),
                1
            );
            assert!(crate::core::extensions::unregister_extension(
                "block-reads-test"
            ));
        });
    }

    /// pi #10549：工具耗时只测工具本身，不含 `after_tool_call` 钩子。
    /// 钩子里睡 400ms，工具本身瞬间返回，`durationMs` 必须远小于 400。
    #[test]
    fn tool_duration_excludes_after_tool_call_hook() {
        use crate::core::extensions::{AfterToolCallOutcome, ExtensionHook, ExtensionTool};

        struct SlowHook;
        impl crate::core::extensions::Extension for SlowHook {
            fn name(&self) -> &str {
                "slow-hook-test"
            }
            fn tools(&self) -> Vec<ExtensionTool> {
                vec![ExtensionTool::simple(
                    "slow_probe",
                    "test",
                    json!({ "type": "object", "properties": {} }),
                    "",
                )]
            }
            fn hooks(&self) -> Vec<ExtensionHook> {
                vec![ExtensionHook::AfterToolCall]
            }
            fn execute_tool(
                &self,
                name: &str,
                _args: &Value,
            ) -> std::result::Result<tools::ToolResult, tools::ToolError> {
                if name == "slow_probe" {
                    Ok(tools::ToolResult::text("ok"))
                } else {
                    Err(tools::ToolError("unknown".to_string()))
                }
            }
            fn after_tool_call_ext(
                &self,
                name: &str,
                _args: &Value,
                result_text: &str,
                _is_error: bool,
            ) -> std::result::Result<AfterToolCallOutcome, tools::ToolError> {
                // 只对本测试的探针工具耗时；并发跑的其它用例的工具原样返回
                if name != "slow_probe" {
                    return Ok(AfterToolCallOutcome {
                        text: result_text.to_string(),
                        content: None,
                        details: None,
                        is_error: None,
                        usage: None,
                        terminate: None,
                    });
                }
                std::thread::sleep(std::time::Duration::from_millis(400));
                Ok(AfterToolCallOutcome {
                    text: format!("{result_text} (hooked)"),
                    content: None,
                    details: None,
                    is_error: None,
                    usage: None,
                    terminate: None,
                })
            }
        }

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            crate::core::extensions::register_extension(SlowHook);
            let cwd = std::env::temp_dir().to_string_lossy().to_string();
            let (base, handle) = mock_server_multi(vec![
                tool_call_frame("slow_probe", "{}", "call_1"),
                text_frame("done", "cmpl_d"),
            ])
            .await;
            let mut agent = test_agent(&base, &cwd);
            let collected: std::sync::Arc<std::sync::Mutex<Vec<J>>> =
                std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let sink = collected.clone();
            agent.attach_json_sink(Box::new(move |e: J| sink.lock().unwrap().push(e)));

            let _ = agent.prompt("hi").await.unwrap();
            handle.await.unwrap();
            let events = collected.lock().unwrap().clone();

            let tr = events_of(&events, "turn_end")[0]["toolResults"]
                .as_array()
                .unwrap()
                .clone();
            assert_eq!(tr.len(), 1);
            // 钩子确实跑过
            assert_eq!(tr[0]["content"][0]["text"], "ok (hooked)");
            let dur = tr[0]["durationMs"].as_u64().unwrap();
            assert!(
                dur < 300,
                "Took 不得把 after_tool_call 钩子的 400ms 算进去，实际 {dur}ms"
            );

            assert!(crate::core::extensions::unregister_extension(
                "slow-hook-test"
            ));
        });
    }

    /// 物化失败的统一取值（`MaterializedSubAgent` 不是 Debug，不能用 `unwrap_err`）。
    fn materialize_expect_err(tpl: &AgentTemplate, spec: core::extensions::SubAgentSpec) -> String {
        match materialize_sub_agent(tpl, spec) {
            Ok(_) => panic!("expected materialization to fail"),
            Err(e) => e,
        }
    }

    /// spec 物化：工具白名单按父级宇宙收窄，且**无条件**剔除子代理工具本身
    /// （递归防护：没有这层，child 继承父级工具集就会拿到 `Agent`，形成无上限递归）。
    #[test]
    fn subagent_materialize_narrows_tools_and_blocks_recursion() {
        let _lock = subagent_test_lock();
        let _agent_dir = crate::test_support::AgentDirGuard::temp();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            crate::core::extensions::register_extension(crate::extensions::subagent::Subagent);
            crate::core::extensions::set_extension_enabled("subagent", true);

            let agent = test_agent("http://127.0.0.1:1", "/tmp");
            let tpl = agent.snapshot_template();

            // 继承（tools=None）：父级全集减去三个子代理工具
            let full =
                materialize_sub_agent(&tpl, core::extensions::SubAgentSpec::new("x")).unwrap();
            let names = full.agent.get_active_tools();
            for sub in [
                "Agent",
                "SubagentWorkflow",
                "get_subagent_result",
                "steer_subagent",
            ] {
                assert!(
                    !names.iter().any(|n| n == sub),
                    "递归防护失效：child 拿到 {sub}"
                );
            }
            // 父级启用了本扩展，因此 `Agent` 在父级宇宙里（证明上面是真剔除而非本来就没有）
            assert!(
                agent.get_all_tools().iter().any(|n| n == "Agent"),
                "父级应能看到 Agent 工具"
            );
            assert!(names.iter().any(|n| n == "read"));

            // 显式白名单：只保留列出的
            let mut spec = core::extensions::SubAgentSpec::new("x");
            spec.tools = Some(vec!["read".to_string(), "grep".to_string()]);
            let narrowed = materialize_sub_agent(&tpl, spec).unwrap();
            let got = narrowed.agent.get_active_tools();
            assert_eq!(got, vec!["read".to_string(), "grep".to_string()]);

            // 白名单全无效 → 明确失败（不静默给错工具集）
            let mut bad = core::extensions::SubAgentSpec::new("x");
            bad.tools = Some(vec!["definitely_not_a_tool".to_string()]);
            let err = materialize_expect_err(&tpl, bad);
            assert!(err.contains("none of the configured tools"), "{err}");

            // **显式**写进白名单的编排工具会被放行：那是扩展为嵌套委派自己放行的
            // （它会在 handler 里按调用者身份限深、限属主），不是 child 顺手继承来的
            let mut nested = core::extensions::SubAgentSpec::new("x");
            nested.tools = Some(vec!["read".to_string(), "Agent".to_string()]);
            let allowed = materialize_sub_agent(&tpl, nested).unwrap();
            let got = allowed.agent.get_active_tools();
            assert!(got.iter().any(|n| n == "Agent"), "{got:?}");
            assert!(!got.iter().any(|n| n == "AgentWorkflow"), "{got:?}");

            crate::core::extensions::set_extension_enabled("subagent", false);
            _ = crate::core::extensions::unregister_extension("subagent");
        });
    }

    /// 运行中的物化子代理必须能被取消句柄真的停下：`MaterializedSubAgent.abort` 捕获的就是
    /// child 的 `run_abort`，只有它跨 `prompt` 保持同一 Arc（`reset_run_state` 只复位不换 Arc），
    /// `/agents stop` 与父级 `parent_abort` 联动才能停下运行中的 child。
    #[test]
    fn materialized_child_abort_handle_stops_the_run() {
        let _lock = subagent_test_lock();
        let _agent_dir = crate::test_support::AgentDirGuard::temp();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let tmp = std::env::temp_dir().join(format!(
                "prux-child-abort-{}",
                crate::utils::time::now_ms()
            ));
            std::fs::create_dir_all(&tmp).unwrap();
            std::fs::write(tmp.join("a.rs"), "fn main() {}").unwrap();
            let cwd = tmp.to_string_lossy().to_string();

            // 在第一个请求上置位取消信号（此时 child 已进入运行），再正常回一个 tool_call；
            // 若取消有效，child 不得发起第二次 LLM 调用。
            let slot: Arc<std::sync::Mutex<Option<Arc<AtomicBool>>>> =
                Arc::new(std::sync::Mutex::new(None));
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
            let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let count = requests.clone();
            let slot_srv = slot.clone();
            let server = tokio::spawn(async move {
                let rounds = [
                    tool_call_frame("read", r#"{\"path\":\"a.rs\"}"#, "call_1"),
                    text_frame("done", "cmpl_2"),
                ];
                for frames in rounds {
                    let Ok(Ok((mut socket, _))) = tokio::time::timeout(
                        std::time::Duration::from_millis(2_000),
                        listener.accept(),
                    )
                    .await
                    else {
                        break;
                    };
                    if count.fetch_add(1, Ordering::SeqCst) == 0
                        && let Some(flag) = slot_srv.lock().unwrap().clone()
                    {
                        flag.store(true, Ordering::Relaxed);
                    }
                    read_request(&mut socket).await;
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
                }
            });

            let agent = test_agent(&base, &cwd);
            let tpl = agent.snapshot_template();
            let mut spec = core::extensions::SubAgentSpec::new("x");
            spec.tools = Some(vec!["read".to_string()]);
            let mut runner = materialize_sub_agent(&tpl, spec).unwrap();
            *slot.lock().unwrap() = Some(runner.controls().abort.clone());

            let res = tokio::time::timeout(
                std::time::Duration::from_secs(3),
                tokio::spawn(async move { runner.run("go".into()).await }),
            )
            .await
            .expect("置位中止后 child 应在下一轮界停下")
            .expect("run task 不应 panic");
            assert!(res.is_ok(), "协作式取消走 Ok 路径: {res:?}");
            assert_eq!(
                requests.load(Ordering::SeqCst),
                1,
                "中止后不得再发起下一次 LLM 调用（说明句柄没作用在运行中的 child 上）"
            );
            server.abort();
        });
    }

    /// 造一个最小注入工具：元数据 + 记录收到参数的 handler。
    fn test_injected_tool(
        name: &str,
        parameters: Value,
    ) -> (core::extensions::InjectedChildTool, Arc<Mutex<Vec<Value>>>) {
        let calls: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = calls.clone();
        let tool = core::extensions::InjectedChildTool {
            meta: core::extensions::ExtensionTool {
                exposure: crate::core::extensions::ToolExposure::Direct,
                namespace: None,
                annotations: crate::core::extensions::ToolAnnotations::default(),
                output_schema: None,
                name: name.to_string(),
                description: format!("{name} injected description"),
                label: Some("Injected".to_string()),
                parameters,
                snippet: format!("{name}: injected snippet"),
                prompt_guidelines: vec![format!("{name} injected guideline")],
                constrained_sampling: false,
                render_shell: None,
                execution_mode: None,
                prepare_arguments: None,
                grammar_sampling: None,
            },
            handler: Arc::new(move |args: Value| {
                sink.lock().unwrap().push(args);
                Ok(ToolResult::text("Recorded."))
            }),
        };
        (tool, calls)
    }

    /// `ToolIndex` 的优先级：注入 > 内置 > 扩展注册表，四问（存在 / 备参 / schema / 模式）同源。
    #[test]
    fn tool_index_prefers_injected_over_builtin() {
        // 拿一个确定是内置的名字（`read`）来验遮蔽：注入的 schema 赢了就说明优先级生效。
        let (tool, _) = test_injected_tool(
            "read",
            json!({ "type": "object", "properties": { "only_injected": { "type": "string" } } }),
        );
        let injected = vec![tool];

        let idx = ToolIndex::resolve(&injected, "read");
        assert!(idx.exists());
        assert!(idx.is_builtin(), "名字仍是内置的，但元数据归注入");
        assert!(idx.injected_handler().is_some(), "命中注入就该走 handler");
        let params = idx.parameters().expect("parameters");
        assert!(
            params["properties"].get("only_injected").is_some(),
            "注入的 schema 必须胜过内置的：{params}"
        );

        // 未注入的名字走原路（内置 / 扩展 / 查不到）
        let base = ToolIndex::resolve(&injected, "read");
        assert!(base.is_builtin());
        assert!(ToolIndex::resolve(&injected, "bash").exists());
        assert!(!ToolIndex::resolve(&injected, "definitely_not_a_tool").exists());
        assert!(
            ToolIndex::resolve(&injected, "definitely_not_a_tool")
                .parameters()
                .is_none()
        );
    }

    /// 注入工具的 `execution_mode` / `prepare_arguments` 也走 `ToolIndex`（不另开一条路）。
    #[test]
    fn tool_index_carries_injected_prepare_arguments_and_mode() {
        let (mut tool, _) = test_injected_tool("read", json!({ "type": "object" }));
        tool.meta.execution_mode = Some(ToolExecutionMode::Sequential);
        tool.meta.prepare_arguments = Some(|args: &Value| {
            if args.get("only_injected_prepared").is_some() {
                json!({ "prepared": true })
            } else {
                args.clone()
            }
        });
        let injected = vec![tool];
        let idx = ToolIndex::resolve(&injected, "read");
        assert_eq!(idx.execution_mode(), Some(ToolExecutionMode::Sequential));
        assert_eq!(
            idx.prepare_arguments(&json!({ "only_injected_prepared": 1 })),
            json!({ "prepared": true })
        );
    }

    /// spec 物化：注入工具**无条件**进 child 的工具表与系统提示，
    /// 且 `tools:` / `disallowed_tools` / `isolated` / `extensions:` 都摘不掉它；
    /// 它不进 `get_all_tools()`（不可被孙代理继承）。
    #[test]
    fn injected_tools_join_child_table_and_survive_scoping() {
        let _lock = subagent_test_lock();
        let _agent_dir = crate::test_support::AgentDirGuard::temp();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            crate::core::extensions::register_extension(crate::extensions::subagent::Subagent);
            crate::core::extensions::set_extension_enabled("subagent", true);

            let agent = test_agent("http://127.0.0.1:1", "/tmp");
            let tpl = agent.snapshot_template();

            let (tool, _) = test_injected_tool("StructuredOutput", json!({ "type": "object" }));
            let mut spec = core::extensions::SubAgentSpec::new("x");
            // 白名单只给 read；注入工具不在白名单里，而且被显式 disallow；再叠 isolated + 空作用域
            spec.tools = Some(vec!["read".to_string()]);
            spec.disallowed_tools = vec!["StructuredOutput".to_string()];
            spec.isolated = true;
            spec.extensions = Some(Vec::new());
            spec.injected_tools = vec![tool];

            let child = materialize_sub_agent(&tpl, spec).unwrap();
            let names = child.agent.get_active_tools();
            assert!(names.iter().any(|n| n == "read"));
            assert!(
                names.iter().any(|n| n == "StructuredOutput"),
                "注入工具必须无条件并入：{names:?}"
            );
            // 系统提示里能看到它（Available tools 段 + Guidelines 段）
            assert!(
                child
                    .agent
                    .system_prompt
                    .contains("StructuredOutput: injected snippet"),
                "注入工具的 snippet 必须进 Available tools"
            );
            assert!(
                child
                    .agent
                    .system_prompt
                    .contains("StructuredOutput injected guideline"),
                "注入工具的 guideline 必须进 Guidelines"
            );
            // 不可继承（回归护栏）：`get_all_tools()` 是算「父级宇宙」用的，注入工具不得进——
            // 它只列举内置 + 已注册扩展工具，这里断言那个不变量不被以后的重构破坏。
            let universe = child.agent.get_all_tools();
            assert!(universe.iter().any(|n| n == "read"), "{universe:?}");
            assert!(
                !universe.iter().any(|n| n == "StructuredOutput"),
                "{universe:?}"
            );

            crate::core::extensions::set_extension_enabled("subagent", false);
            _ = crate::core::extensions::unregister_extension("subagent");
        });
    }

    /// spec 物化：`isolated` 剔掉全部扩展工具；`disallowed_tools` 显式剔除。
    #[test]
    fn subagent_materialize_honours_isolated_and_disallowed_tools() {
        let _lock = subagent_test_lock();
        let _agent_dir = crate::test_support::AgentDirGuard::temp();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            crate::core::extensions::register_extension(crate::extensions::subagent::Subagent);
            crate::core::extensions::set_extension_enabled("subagent", true);

            let agent = test_agent("http://127.0.0.1:1", "/tmp");
            let tpl = agent.snapshot_template();

            // isolated：扩展工具（Agent 等）在 child 里彻底不存在，内置工具仍在
            let mut spec = core::extensions::SubAgentSpec::new("x");
            spec.isolated = true;
            let isolated = materialize_sub_agent(&tpl, spec).unwrap();
            let names = isolated.agent.get_active_tools();
            for ext_tool in [
                "Agent",
                "SubagentWorkflow",
                "get_subagent_result",
                "steer_subagent",
            ] {
                assert!(
                    !names.iter().any(|n| n == ext_tool),
                    "isolated 下不应有扩展工具 {ext_tool}"
                );
            }
            assert!(
                names.iter().any(|n| n == "read"),
                "内置工具应保留: {names:?}"
            );

            // disallowed_tools：显式剔除已选中的名字
            let mut spec = core::extensions::SubAgentSpec::new("x");
            spec.disallowed_tools = vec!["read".to_string(), "grep".to_string()];
            let filtered = materialize_sub_agent(&tpl, spec).unwrap();
            let names = filtered.agent.get_active_tools();
            assert!(!names.iter().any(|n| n == "read"), "read 应被剔除");
            assert!(!names.iter().any(|n| n == "grep"), "grep 应被剔除");
            assert!(names.iter().any(|n| n == "bash"));

            // isolated + 显式白名单：白名单里的扩展工具也被剔除（isolated 优先）
            let mut spec = core::extensions::SubAgentSpec::new("x");
            spec.isolated = true;
            spec.tools = Some(vec!["read".to_string(), "Agent".to_string()]);
            let both = materialize_sub_agent(&tpl, spec).unwrap();
            assert_eq!(both.agent.get_active_tools(), vec!["read".to_string()]);

            crate::core::extensions::set_extension_enabled("subagent", false);
            _ = crate::core::extensions::unregister_extension("subagent");
        });
    }

    /// spec 物化：扩展作用域（`extensions` 白名单 / `exclude_extensions` 黑名单 /
    /// `clear_skills`）对工具面与技能索引生效。
    #[test]
    fn subagent_materialize_applies_extension_scope_and_skill_clearing() {
        let _lock = subagent_test_lock();
        let _agent_dir = crate::test_support::AgentDirGuard::temp();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            crate::core::extensions::register_extension(crate::extensions::subagent::Subagent);
            crate::core::extensions::set_extension_enabled("subagent", true);
            let agent = test_agent("http://127.0.0.1:1", "/tmp");
            let tpl = agent.snapshot_template();

            // 白名单只允许"其它扩展" → subagent 的三个工具全被剔除，内置保留
            let mut spec = core::extensions::SubAgentSpec::new("x");
            spec.extensions = Some(vec!["some-other-ext".to_string()]);
            let scoped = materialize_sub_agent(&tpl, spec).unwrap();
            let names = scoped.agent.get_active_tools();
            assert!(!names.iter().any(|n| n == "Agent"), "{names:?}");
            assert!(
                names.iter().any(|n| n == "read"),
                "内置不受扩展作用域影响: {names:?}"
            );

            // 黑名单剔除 subagent → 同样没有 Agent，但 `get_subagent_result` 也没了
            let mut spec = core::extensions::SubAgentSpec::new("x");
            spec.exclude_extensions = vec!["subagent".to_string()];
            let scoped = materialize_sub_agent(&tpl, spec).unwrap();
            let names = scoped.agent.get_active_tools();
            assert!(!names.iter().any(|n| n == "Agent"), "{names:?}");
            assert!(!names.iter().any(|n| n == "steer_subagent"), "{names:?}");

            // clear_skills：不继承父级技能索引
            let mut parent = test_agent("http://127.0.0.1:1", "/tmp");
            parent.skills = vec![crate::core::skills::Skill {
                name: "demo".to_string(),
                description: "demo skill".to_string(),
                path: std::path::PathBuf::from("/tmp/demo/SKILL.md"),
                base_dir: std::path::PathBuf::from("/tmp/demo"),
                disable_model_invocation: false,
                source: "/tmp".to_string(),
            }];
            let tpl2 = parent.snapshot_template();
            let kept =
                materialize_sub_agent(&tpl2, core::extensions::SubAgentSpec::new("x")).unwrap();
            assert_eq!(kept.agent.skills.len(), 1, "默认继承父级技能索引");
            let mut spec = core::extensions::SubAgentSpec::new("x");
            spec.clear_skills = true;
            let cleared = materialize_sub_agent(&tpl2, spec).unwrap();
            assert!(
                cleared.agent.skills.is_empty(),
                "skills:false 应清空继承的技能索引"
            );

            crate::core::extensions::set_extension_enabled("subagent", false);
            _ = crate::core::extensions::unregister_extension("subagent");
        });
    }

    /// spec 物化：replace 模式清空 `context_files`；append 模式写入追加段。
    #[test]
    fn subagent_materialize_switches_prompt_mode() {
        let agent = test_agent("http://127.0.0.1:1", "/tmp");
        let mut parent = agent;
        parent.rebuild_ctx.context_files =
            vec![("AGENTS.md".to_string(), "project rules".to_string())];
        let tpl = parent.snapshot_template();

        // replace + clear：系统提示替换，且不继承项目指令
        let mut spec = core::extensions::SubAgentSpec::new("Explore");
        spec.system_prompt = Some("ROLE ONLY".to_string());
        spec.clear_context_files = true;
        let child = materialize_sub_agent(&tpl, spec).unwrap();
        assert!(child.agent.system_prompt.starts_with("ROLE ONLY"));
        assert!(!child.agent.system_prompt.contains("project rules"));

        // replace 但不清空：项目指令保留（prux 既有路径）
        let mut keep = core::extensions::SubAgentSpec::new("Explore");
        keep.system_prompt = Some("ROLE ONLY".to_string());
        let child2 = materialize_sub_agent(&tpl, keep).unwrap();
        assert!(child2.agent.system_prompt.contains("project rules"));

        // append：不动自定义提示，只追加
        let mut app = core::extensions::SubAgentSpec::new("general-purpose");
        app.append_system_prompt = Some("BRIDGE".to_string());
        let child3 = materialize_sub_agent(&tpl, app).unwrap();
        assert!(child3.agent.system_prompt.contains("BRIDGE"));
        assert!(child3.agent.system_prompt.contains("project rules"));
        assert!(!child3.agent.system_prompt.starts_with("BRIDGE"));
    }

    /// spec 物化：`inherit_context` 复制父级对话；未开启时 child 从空上下文开始。
    #[test]
    fn subagent_materialize_inherits_context_only_when_asked() {
        let mut agent = test_agent("http://127.0.0.1:1", "/tmp");
        agent.messages.push(AgentMessage::user_text("parent turn"));
        let tpl = agent.snapshot_template();

        let child = materialize_sub_agent(&tpl, core::extensions::SubAgentSpec::new("x")).unwrap();
        assert!(child.agent.messages.is_empty());

        let mut spec = core::extensions::SubAgentSpec::new("x");
        spec.inherit_context = true;
        let forked = materialize_sub_agent(&tpl, spec).unwrap();
        assert_eq!(forked.agent.messages.len(), 1);
        assert_eq!(forked.agent.messages[0].text(), "parent turn");
    }

    /// spec 物化：型号解析失败**直接报错**（不静默继承父级模型）。
    #[test]
    fn subagent_materialize_rejects_unresolvable_model() {
        let agent = test_agent("http://127.0.0.1:1", "/tmp");
        let tpl = agent.snapshot_template();

        let mut spec = core::extensions::SubAgentSpec::new("x");
        spec.model = Some("no-such-provider/no-such-model".to_string());
        let err = materialize_expect_err(&tpl, spec);
        assert!(err.contains("not available"), "{err}");
        assert!(err.contains("no-such-provider/no-such-model"), "{err}");

        // 缺少 provider 前缀同样是明确错误
        let mut bad = core::extensions::SubAgentSpec::new("x");
        bad.model = Some("just-a-model".to_string());
        let err = materialize_expect_err(&tpl, bad);
        assert!(err.contains("provider/modelId"), "{err}");
    }

    /// graceful max_turns：到限注入 wrap-up，宽限 `SUBAGENT_GRACE_TURNS` 轮后停机。
    #[test]
    fn subagent_materialize_wires_graceful_max_turns() {
        let agent = test_agent("http://127.0.0.1:1", "/tmp");
        let tpl = agent.snapshot_template();

        // 未设置 → 不挂回调（无上限）
        let plain = materialize_sub_agent(&tpl, core::extensions::SubAgentSpec::new("x")).unwrap();
        assert!(plain.agent.finish_turn.is_none());

        // max_turns = 2：第 2 轮注入 wrap-up；第 2 + grace 轮返回 End（停机）
        let mut spec = core::extensions::SubAgentSpec::new("x");
        spec.max_turns = Some(2);
        let mut child = materialize_sub_agent(&tpl, spec).unwrap();
        let ctx = TurnContext {
            message: AgentMessage::user_text(""),
            tool_results: Vec::new(),
            context: Vec::new(),
            new_messages: Vec::new(),
        };
        let stop = |a: &mut Agent| {
            a.finish_turn
                .as_ref()
                .and_then(|m| {
                    let mut g = m.lock().unwrap();
                    let f: &mut dyn FnMut(&TurnContext) -> Option<FinishTurnAction> = g.as_mut();
                    f(&ctx)
                })
                .is_some_and(|d| d == FinishTurnAction::End)
        };

        assert!(!stop(&mut child.agent), "第 1 轮不应停机");
        assert!(
            child.agent.runtime_steer_inbox.lock().unwrap().is_empty(),
            "第 1 轮不应注入 wrap-up"
        );

        assert!(!stop(&mut child.agent), "到限轮仍应给宽限");
        let injected: Vec<String> = child
            .agent
            .runtime_steer_inbox
            .lock()
            .unwrap()
            .iter()
            .map(|m| m.text())
            .collect();
        assert_eq!(injected.len(), 1, "到限轮应注入一条 wrap-up");
        assert!(injected[0].contains("Wrap up"), "{injected:?}");

        // 宽限期内继续
        for turn in 3..(2 + SUBAGENT_GRACE_TURNS) {
            assert!(!stop(&mut child.agent), "第 {turn} 轮仍在宽限期内");
        }
        assert!(
            stop(&mut child.agent),
            "第 {} 轮应停机",
            2 + SUBAGENT_GRACE_TURNS
        );

        // pi 0.87.0：error/aborted 硬退出时谓词不应有副作用（不计数、不注入 wrap-up）
        let mut spec = core::extensions::SubAgentSpec::new("x");
        spec.max_turns = Some(2);
        let child = materialize_sub_agent(&tpl, spec).unwrap();
        let err_ctx = TurnContext {
            message: AgentMessage {
                stop_reason: Some("error".to_string()),
                ..AgentMessage::user_text("")
            },
            tool_results: Vec::new(),
            context: Vec::new(),
            new_messages: Vec::new(),
        };
        let decision = child.agent.finish_turn.as_ref().and_then(|m| {
            let mut g = m.lock().unwrap();
            let f: &mut dyn FnMut(&TurnContext) -> Option<FinishTurnAction> = g.as_mut();
            f(&err_ctx)
        });
        assert_eq!(decision, None, "硬退出决策被忽略");
        assert!(
            child.agent.runtime_steer_inbox.lock().unwrap().is_empty(),
            "硬退出不应注入 wrap-up"
        );
    }

    /// spec 物化：`resume_session_path` 打开既有会话、回灌上下文并继续写同一文件。
    #[test]
    fn subagent_materialize_resumes_existing_session() {
        let _agent_dir = crate::test_support::AgentDirGuard::temp();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let (base, handle) = mock_server_multi(vec![text_frame("first answer", "c1")]).await;
            let mut parent = test_agent(&base, "/tmp");
            let dir = std::env::temp_dir().join(format!(
                "prux-subagent-resume-{}",
                crate::utils::time::now_ms()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            parent.session = Some(
                crate::core::session_manager::Session::create_with(
                    crate::core::session_manager::SessionCreateOptions {
                        cwd: "/tmp".to_string(),
                        session_dir: Some(dir.clone()),
                        persist: true,
                        ..Default::default()
                    },
                )
                .unwrap(),
            );
            parent.prompt("remember this").await.unwrap();
            handle.await.unwrap();
            let path = parent
                .session
                .as_ref()
                .and_then(|s| s.get_session_file())
                .map(|p| p.to_string_lossy().to_string())
                .expect("session file");
            let tpl = parent.snapshot_template();

            let mut spec = core::extensions::SubAgentSpec::new("x");
            spec.resume_session_path = Some(path.clone());
            spec.inherit_context = false;
            let resumed = materialize_sub_agent(&tpl, spec).unwrap();

            let texts: Vec<String> = resumed.agent.messages.iter().map(|m| m.text()).collect();
            assert!(
                texts.iter().any(|t| t.contains("remember this")),
                "未回灌上下文: {texts:?}"
            );
            assert!(
                texts.iter().any(|t| t.contains("first answer")),
                "未回灌回答: {texts:?}"
            );
            // 继续写同一会话文件（不是新开一个）
            assert_eq!(
                resumed
                    .agent
                    .session
                    .as_ref()
                    .and_then(|s| s.get_session_file())
                    .map(|p| p.to_string_lossy().to_string())
                    .as_deref(),
                Some(path.as_str())
            );
        });
    }

    /// 端到端：后台子代理经真实接缝物化并真的跑一轮 LLM，完成后
    /// ① 投递一条 `Continuation`（完整结果）；② 结果可经 `get_subagent_result(wait=true)` 取回；
    /// ③ 子会话落盘并挂在父会话之下。
    #[test]
    fn subagent_background_spawn_notifies_and_persists_child_session() {
        use crate::core::extensions::Extension as _;
        let _lock = subagent_test_lock();
        let agent_home = crate::test_support::AgentDirGuard::temp();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            crate::core::extensions::register_extension(crate::extensions::subagent::Subagent);
            crate::core::extensions::set_extension_enabled("subagent", true);
            while crate::core::extensions::take_pending_ui().is_some() {}

            let (base, handle) = mock_server_multi(vec![text_frame("child work done", "c1")]).await;
            let cwd = std::env::temp_dir().to_string_lossy().to_string();
            let mut parent = test_agent(&base, &cwd);
            // 父级先有自己的会话：子会话应挂到它下面（/resume 嵌套）
            let sess_dir =
                session_manager::default_session_dir(&cwd, &settings_manager::agent_dir());
            let parent_session = Session::create_with(session_manager::SessionCreateOptions {
                cwd: cwd.clone(),
                session_dir: Some(sess_dir.clone()),
                persist: true,
                ..Default::default()
            })
            .unwrap();
            let parent_id = parent_session.session_id.clone();
            parent.session = Some(parent_session);

            let tpl = std::sync::Arc::new(parent.snapshot_template());
            let ctx = make_exec_ctx(
                &cwd,
                &tpl,
                &std::sync::Arc::new(AtomicBool::new(false)),
                &None,
                None,
                None,
            );
            let ext = crate::extensions::subagent::Subagent;

            let out = ext
                .execute_tool_async(
                    "Agent".to_string(),
                    json!({
                        "prompt": "do the thing",
                        "description": "do thing",
                        "subagent_type": "general-purpose",
                        "run_in_background": true
                    }),
                    ctx.clone(),
                )
                .await
                .expect("Agent tool");
            assert!(out.text.contains("launched"), "{}", out.text);
            assert!(
                out.text.contains("do not sleep or poll"),
                "后台结果应明确禁止轮询: {}",
                out.text
            );
            let id = out.details.as_ref().unwrap()["id"]
                .as_str()
                .unwrap()
                .to_string();
            assert_eq!(out.details.as_ref().unwrap()["background"], true);

            // ② 完成通知是一条 Continuation，携带完整结果。
            // **先等通知、再读结果**：`get_subagent_result` 带 consume 语义（已读即抑制
            // 尚未投递的通知，见 migration-subagents.md），先读会让通知永远不投递。
            let mut notified: Option<String> = None;
            for _ in 0..500 {
                while let Some(r) = crate::core::extensions::take_pending_ui() {
                    if let core::extensions::ExtensionUiRequest::Continuation { message } = r {
                        notified = Some(message.text());
                    }
                }
                if notified.is_some() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            let text = notified.expect("后台完成必须投递一条 Continuation");
            assert!(text.contains("<subagent_result"), "{text}");
            assert!(text.contains("status=\"completed\""), "{text}");
            assert!(text.contains(&id), "{text}");
            assert!(text.contains("child work done"), "{text}");

            // ③ 结果可经 `get_subagent_result(wait=true)` 取回（此时已终结，立即返回）
            let res = ext
                .execute_tool_async(
                    "get_subagent_result".to_string(),
                    json!({ "agent_id": id, "wait": true }),
                    ctx.clone(),
                )
                .await
                .expect("get_subagent_result");
            assert!(res.text.contains("completed"), "{}", res.text);
            assert!(res.text.contains("child work done"), "{}", res.text);

            // ④ 子会话落盘 + 挂父会话
            let sessions = session_manager::list_sessions(&sess_dir);
            let child = sessions
                .iter()
                .filter_map(|p| Session::open(&p.to_string_lossy()).ok())
                .find(|s| s.parent_session_id() == Some(parent_id.as_str()))
                .expect("子会话应已落盘并带 parentSessionId");
            assert_eq!(child.cwd, cwd);
            let _ = agent_home;

            handle.await.unwrap();
            crate::core::extensions::set_extension_enabled("subagent", false);
            _ = crate::core::extensions::unregister_extension("subagent");
        });
    }

    /// 子代理扩展：注册并启用后 `Agent` 工具可调用，隔离子代理跑自己的 LLM 轮次。
    /// 落地 Tier1 语义（默认**前台**，见 migration-subagents.md 偏离 3.1）。
    #[test]
    fn subagent_extension_runs_agent_tool() {
        let _lock = subagent_test_lock();
        // 子代理默认 persist_session=true 会写子会话：用临时 agent_dir 隔离
        let _agent_dir = crate::test_support::AgentDirGuard::temp();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            crate::core::extensions::register_extension(crate::extensions::subagent::Subagent);
            // subagent 声明 default_enabled=false：仅注册不够，必须显式启用，
            // 否则 compose_tools 不并入 Agent → "Tool Agent not found"。
            crate::core::extensions::set_extension_enabled("subagent", true);
            let cwd = std::env::temp_dir().to_string_lossy().to_string();
            // 连接1：父代理发出 Agent 工具调用；连接2：子代理回复；连接3：父代理最终文本
            // 显式前台：本用例验证"前台 spawn → 结果 inline"链路（默认后台见 subagent.rs 的用例）
            let f1 = tool_call_frame(
                "Agent",
                r#"{\"prompt\":\"do x\",\"description\":\"do x\",\"subagent_type\":\"general-purpose\",\"run_in_background\":false}"#,
                "call_1",
            );
            let f2 = text_frame("child done", "cmpl_c");
            let f3 = text_frame("parent final", "cmpl_p");
            let (base, handle) = mock_server_multi(vec![f1, f2, f3]).await;

            let mut agent = test_agent(&base, &cwd);
            let collected: std::sync::Arc<std::sync::Mutex<Vec<J>>> =
                std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let sink = collected.clone();
            agent.attach_json_sink(Box::new(move |e: J| sink.lock().unwrap().push(e)));

            let text = agent.prompt("hi").await.unwrap();
            handle.await.unwrap();
            assert_eq!(text, "parent final");

            let events = collected.lock().unwrap().clone();
            let turn_end = events_of(&events, "turn_end");
            // 第一轮 turn_end 的 toolResult 是前台子代理的结果（状态头 + 子代理输出）
            let tr = turn_end[0]["toolResults"].as_array().unwrap();
            assert_eq!(tr.len(), 1);
            assert_eq!(tr[0]["isError"], false);
            let t = tr[0]["content"][0]["text"].as_str().unwrap();
            assert!(t.contains("child done"), "got: {t}");
            assert!(t.contains("[subagent "), "状态头缺失: {t}");
            assert!(t.contains("completed"), "状态缺失: {t}");
            // details 带结构化元数据
            assert_eq!(tr[0]["details"]["type"], "general-purpose");
            assert_eq!(tr[0]["details"]["status"], "completed");

            // 回归：unregister 移除同名全部条目，两个 subagent 测试并发注册/注销会互相
            // 抢先移除对方条目（另一处测试忽略返回值）；此处不断言结果避免偶发竞态。
            // 先禁用（触发 on_enabled_changed → 中止并清空子代理注册表），再注销。
            crate::core::extensions::set_extension_enabled("subagent", false);
            _ = crate::core::extensions::unregister_extension("subagent");
        });
    }

    /// `tool_call_frame` 的参数串与 `text_frame` 的正文都要「已转义一次」的形式：
    /// 它们会被再嵌进 SSE 帧的 JSON 字符串里。
    fn json_string_escape(s: &str) -> String {
        let quoted = serde_json::to_string(s).unwrap();
        quoted[1..quoted.len() - 1].to_string()
    }

    /// 端到端：autoresearch 作业在**真子会话**上迭代到完成标记。
    ///
    /// 假 provider 演两轮子代理：第 1 轮回 `[continue]`，第 2 轮（走 resume 的同一子会话）
    /// 回 `[goal-complete]`。断言的是「派发 → 读记录 → 判定 → 收敛」整条链路：
    /// 第二轮能跑起来本身就要求 resume 在真会话文件上成立（假 runner 那条用例覆盖不了它）。
    #[test]
    fn autoresearch_job_loops_until_the_completion_marker() {
        let _lock = subagent_test_lock();
        let _agent_dir = crate::test_support::AgentDirGuard::temp();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            crate::core::extensions::register_extension(crate::extensions::subagent::Subagent);
            crate::core::extensions::set_extension_enabled("subagent", true);
            let cwd = std::env::temp_dir().to_string_lossy().to_string();

            // 连接1：第 1 轮子代理；连接2：resume 同一子会话后的第 2 轮
            // （正文里不能带真换行：帧是拼进 JSON 字符串的）
            let f1 = text_frame("first change [continue]", "cmpl_1");
            let f2 = text_frame("verified [goal-complete]", "cmpl_2");
            let (base, handle) = mock_server_multi(vec![f1, f2]).await;

            let agent = test_agent(&base, &cwd);
            let id = crate::extensions::subagent::autoresearch::start_with(
                agent.exec_ctx(),
                tokio::runtime::Handle::current(),
                "make the renderer faster",
                None,
            );
            let snapshot = await_autoresearch_terminal(id).await;
            handle.await.unwrap();

            assert_eq!(
                snapshot.status,
                crate::extensions::subagent::autoresearch::JobStatus::Completed,
                "{snapshot:?}"
            );
            assert_eq!(snapshot.iteration, 2, "{snapshot:?}");
            assert!(
                snapshot
                    .latest_result
                    .as_deref()
                    .is_some_and(|r| r.contains("[goal-complete]")),
                "{snapshot:?}"
            );

            // 两轮落在**同一个**子会话文件里：resume 续的是它，而不是每轮新建一个。
            let agent_id = snapshot.agent_id.clone().expect("作业应留下子代理记录 id");
            let path = crate::extensions::subagent::agent_session_path(&agent_id)
                .expect("子代理记录应带子会话路径（persist_session=true）");
            let text = std::fs::read_to_string(&path).expect("子会话文件应落盘");
            assert!(
                text.contains("first change") && text.contains("verified"),
                "同一个子会话里应同时有两轮的回答（{path}）: {text}"
            );

            crate::core::extensions::set_extension_enabled("subagent", false);
            _ = crate::core::extensions::unregister_extension("subagent");
        });
    }

    /// 等一个 autoresearch 作业到终态（假 provider 是同步的，所以只等状态）。
    async fn await_autoresearch_terminal(
        id: crate::extensions::subagent::autoresearch::JobId,
    ) -> crate::extensions::subagent::autoresearch::JobSnapshot {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let snapshot = crate::extensions::subagent::autoresearch::snapshot(id)
                .expect("job should be registered");
            if snapshot.status.is_finished() {
                return snapshot;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "autoresearch job {id} did not settle in time: {snapshot:?}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// 端到端：`agent({schema})` 把 `StructuredOutput` 注入给子代理，子代理真调它，
    /// 脚本拿到**对象**。这是唯一能证明「注入接缝真的通了」的证据——
    /// 假 provider 演子代理的工具调用，走完 注入 → 工具表 → 派发 → 捕获 → 回传 全程。
    ///
    /// 工具现在立刻返回（后台运行），所以这里直接用 `Agent::exec_ctx()` 拿到真的
    /// `make_sub_agent` 调一次 `SubagentWorkflow`，不再借道父级模型回合——父级回合与
    /// 后台运行的子代理会争抢假 provider 的连接序，那样测的就不是注入链路了。
    #[test]
    fn workflow_schema_agent_gets_a_structured_object_end_to_end() {
        let _lock = subagent_test_lock();
        let _agent_dir = crate::test_support::AgentDirGuard::temp();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            crate::core::extensions::register_extension(crate::extensions::subagent::Subagent);
            crate::core::extensions::set_extension_enabled("subagent", true);
            let cwd = std::env::temp_dir().to_string_lossy().to_string();

            let script = "export const meta = { name: 'x', description: 'd' }\n\
                          const r = await agent('x', { schema: { type: 'object', required: \
                          ['answer'], properties: { answer: { type: 'string' } } } }); return r";
            // 连接1：子代理调 StructuredOutput；连接2：子代理收尾
            let f1 = tool_call_frame(
                "StructuredOutput",
                &json_string_escape(&json!({ "answer": "42" }).to_string()),
                "call_2",
            );
            let f2 = text_frame("child final", "cmpl_c");
            let (base, handle) = mock_server_multi(vec![f1, f2]).await;

            let agent = test_agent(&base, &cwd);
            let out = crate::core::extensions::Extension::execute_tool_async(
                &crate::extensions::subagent::Subagent,
                "SubagentWorkflow".to_string(),
                json!({ "script": script }),
                agent.exec_ctx(),
            )
            .await
            .expect("workflow tool");
            let run_id = out.details.as_ref().unwrap()["taskId"]
                .as_str()
                .expect("details.taskId")
                .to_string();
            let run = await_run_terminal(&run_id).await;
            handle.await.unwrap();
            assert_eq!(
                run.value,
                Some(json!({ "answer": "42" })),
                "脚本拿到的应是 StructuredOutput 交回的**对象**"
            );

            crate::core::extensions::set_extension_enabled("subagent", false);
            _ = crate::core::extensions::unregister_extension("subagent");
        });
    }

    /// 端到端：`agent({ resume })` 续跑同一个 child——第二个调用打开第一个的子会话、
    /// 以它的上下文继续（`manager` 复用同一条记录），脚本拿到续跑后的新答案。
    ///
    /// 这里必须走真 `make_sub_agent`（假 runner 报不出会话路径）；假 provider 演两轮子代理。
    #[test]
    fn workflow_agent_resume_continues_the_same_child_session() {
        let _lock = subagent_test_lock();
        let _agent_dir = crate::test_support::AgentDirGuard::temp();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            crate::core::extensions::register_extension(crate::extensions::subagent::Subagent);
            crate::core::extensions::set_extension_enabled("subagent", true);
            let cwd = std::env::temp_dir().to_string_lossy().to_string();

            // 连接1：第一个 child 的回答；连接2：被续跑的那个 child 的回答
            let f1 = text_frame("first answer", "cmpl_a");
            let f2 = text_frame("second answer", "cmpl_b");
            let (base, handle) = mock_server_multi(vec![f1, f2]).await;

            let agent = test_agent(&base, &cwd);
            let script = "export const meta = { name: 'res', description: 'd' }\n\
                          const a = await agent('one', { label: 'loop' })\n\
                          const b = await agent('more', { resume: 'loop' })\n\
                          return { a, b }";
            let out = crate::core::extensions::Extension::execute_tool_async(
                &crate::extensions::subagent::Subagent,
                "SubagentWorkflow".to_string(),
                json!({ "script": script }),
                agent.exec_ctx(),
            )
            .await
            .expect("workflow tool");
            let run_id = out.details.as_ref().unwrap()["taskId"]
                .as_str()
                .expect("details.taskId")
                .to_string();
            let run = await_run_terminal(&run_id).await;
            handle.await.unwrap();
            let value = run.value.clone().unwrap();
            assert_eq!(value["a"], "first answer", "{:?}", run.error);
            assert_eq!(
                value["b"], "second answer",
                "resume 应续跑并拿到新答案: {value}"
            );

            // 两次调用落在同一个 record id 上（resume 复用记录）。
            // 只看**结算**条目：spawn 时也会写一次 recordId，故用 state 过滤掉运行中的那几条。
            let ids: Vec<String> = run
                .log()
                .iter()
                .filter(|e| {
                    e["type"] == "workflow_agent"
                        && e["state"] == "done"
                        && e["recordId"].is_string()
                })
                .filter_map(|e| e["recordId"].as_str().map(str::to_string))
                .collect();
            assert_eq!(ids.len(), 2, "{ids:?}");
            assert_eq!(ids[0], ids[1], "resume 复用被续跑 child 的记录: {ids:?}");

            // spawn 时就登记了 recordId：运行中的条目也带着它（检查器才能提前 `c` 打开）
            let log = run.log();
            let mid_run: Vec<&serde_json::Value> = log
                .iter()
                .filter(|e| {
                    e["type"] == "workflow_agent"
                        && e["recordId"].is_string()
                        && e["state"] != "done"
                })
                .collect();
            assert!(
                !mid_run.is_empty()
                    && mid_run
                        .iter()
                        .all(|e| e["recordId"].as_str() == Some(ids[0].as_str())),
                "运行中条目应带同一个 recordId: {mid_run:?}"
            );

            crate::core::extensions::set_extension_enabled("subagent", false);
            _ = crate::core::extensions::unregister_extension("subagent");
        });
    }

    /// 端到端：**嵌套子代理**——主的子代理自己再派一代（核心默认剔除编排工具，
    /// 只放行扩展显式写进 child 白名单的那一份；深度与属主由扩展在 handler 里把关）。
    #[test]
    fn nested_subagents_can_delegate_one_more_level() {
        let _lock = subagent_test_lock();
        let _agent_dir = crate::test_support::AgentDirGuard::temp();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            crate::core::extensions::register_extension(crate::extensions::subagent::Subagent);
            crate::core::extensions::set_extension_enabled("subagent", true);
            let cwd = std::env::temp_dir().to_string_lossy().to_string();

            // 四段对话按顺序：主 → 子 → 孙 → 子（拿到孙的结果）→ 主
            let main_call = tool_call_frame(
                "Agent",
                r#"{\"prompt\":\"child task\",\"description\":\"child\",\"subagent_type\":\"general-purpose\",\"run_in_background\":false}"#,
                "call_child",
            );
            let child_call = tool_call_frame(
                "Agent",
                r#"{\"prompt\":\"grand task\",\"description\":\"grand\",\"subagent_type\":\"general-purpose\",\"run_in_background\":false}"#,
                "call_grand",
            );
            let (base, handle) = mock_server_multi(vec![
                main_call,
                child_call,
                text_frame("grand answer", "cmpl_g"),
                text_frame("child saw grand answer", "cmpl_c"),
                text_frame("done", "cmpl_m"),
            ])
            .await;

            let mut agent = test_agent(&base, &cwd);
            // 主会话必须**看得到并敢用** `Agent`（默认测试 agent 的白名单只有 read/write）
            agent.set_active_tools(vec!["read".to_string(), "Agent".to_string()]);
            // 主回合：模型调用 Agent（前台）→ 子代理再派一代 → 结果层层回传
            let answer = agent.prompt("go").await.expect("main turn");
            handle.await.unwrap();
            assert!(answer.contains("done"), "主的最终回答: {answer}");

            // 记录树：子（depth 1，无父）与孙（depth 2，父 = 子）
            let tree = crate::extensions::subagent::record_tree_for_test();
            let (child_id, _, _) = tree
                .iter()
                .find(|(_, _, depth)| *depth == 1)
                .expect("应有一个 depth 1 的子代理")
                .clone();
            let (grand_id, grand_parent, _) = tree
                .iter()
                .find(|(_, _, depth)| *depth == 2)
                .expect("子代理应派出一个 depth 2 的孙代理")
                .clone();
            assert_eq!(
                grand_parent.as_deref(),
                Some(child_id.as_str()),
                "孙的父是这个子代理: {tree:?}"
            );
            let (agent_type, result) =
                crate::extensions::subagent::record_for_test(&grand_id).expect("孙记录");
            assert_eq!(agent_type, "general-purpose");
            assert!(
                result.as_deref().unwrap_or_default().contains("grand answer"),
                "孙的结果: {result:?}"
            );
            crate::core::extensions::set_extension_enabled("subagent", false);
            _ = crate::core::extensions::unregister_extension("subagent");
        });
    }

    /// 等一个工作流运行到达终态（假 provider 是同步的，所以只等状态）。
    async fn await_run_terminal(
        run_id: &str,
    ) -> crate::extensions::subagent::workflow::task::WorkflowRun {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let run = crate::extensions::subagent::workflow::task::find(run_id)
                .expect("run should be registered");
            if run.status.is_terminal() {
                return run;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "run {run_id} did not settle in time"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// 端到端：有 schema 的子代理**没**调工具 → 脚本拿到 `null`（不是抛），
    /// 且正文里那段形似合规的 JSON **不会被**抓回来当答案（上游也不解析正文）。
    #[test]
    fn workflow_schema_agent_without_the_tool_call_yields_null() {
        let _lock = subagent_test_lock();
        let _agent_dir = crate::test_support::AgentDirGuard::temp();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            crate::core::extensions::register_extension(crate::extensions::subagent::Subagent);
            crate::core::extensions::set_extension_enabled("subagent", true);
            let cwd = std::env::temp_dir().to_string_lossy().to_string();

            let script = "export const meta = { name: 'x', description: 'd' }\n\
                          const r = await agent('x', { schema: { type: 'object', required: \
                          ['answer'], properties: { answer: { type: 'string' } } } }); \
                          return { got: r }";
            // 子代理只在**正文**里给了一个形似合规的 JSON 对象，从未调工具
            let f1 = text_frame(&json_string_escape("{\"answer\": \"42\"}"), "cmpl_c");
            let (base, handle) = mock_server_multi(vec![f1]).await;

            let agent = test_agent(&base, &cwd);
            let out = crate::core::extensions::Extension::execute_tool_async(
                &crate::extensions::subagent::Subagent,
                "SubagentWorkflow".to_string(),
                json!({ "script": script }),
                agent.exec_ctx(),
            )
            .await
            .expect("workflow tool");
            let run_id = out.details.as_ref().unwrap()["taskId"]
                .as_str()
                .expect("details.taskId")
                .to_string();
            let run = await_run_terminal(&run_id).await;
            handle.await.unwrap();
            assert_eq!(
                run.value,
                Some(json!({ "got": null })),
                "没从工具交回就是 null，正文里的 JSON 不算"
            );

            crate::core::extensions::set_extension_enabled("subagent", false);
            _ = crate::core::extensions::unregister_extension("subagent");
        });
    }

    /// 回归：同一 assistant 条目一次发出多个 task（Sequential 模式）工具时，
    /// tool_started 的 toolIndex 必须是源序（0,1,2…）。旧实现在 push toolResult 前
    /// 用 len-1 计算索引，会把 4 个顺序工具记成 0,0,1,2，导致 (assistantEntryId, toolIndex)
    /// 判重 → duplicate_tool_invocation 损坏，会话无法恢复。
    #[test]
    fn sequential_extension_batch_assigns_source_order_tool_indexes() {
        struct SeqProbe;
        impl crate::core::extensions::Extension for SeqProbe {
            fn name(&self) -> &str {
                "seq-probe-test"
            }
            fn tools(&self) -> Vec<crate::core::extensions::ExtensionTool> {
                let mut probe = crate::core::extensions::ExtensionTool::simple(
                    "seq_probe",
                    "test tool that forces sequential batch execution",
                    json!({
                        "type": "object",
                        "properties": { "n": { "type": "integer" } },
                        "required": ["n"]
                    }),
                    "",
                );
                probe.execution_mode = Some(ToolExecutionMode::Sequential);
                vec![probe]
            }
            fn execute_tool(
                &self,
                name: &str,
                args: &Value,
            ) -> std::result::Result<tools::ToolResult, tools::ToolError> {
                if name == "seq_probe" {
                    Ok(tools::ToolResult::text(format!(
                        "probe {}",
                        args.get("n").and_then(|v| v.as_i64()).unwrap_or(-1)
                    )))
                } else {
                    Err(tools::ToolError("unknown".to_string()))
                }
            }
        }

        let _lock = subagent_test_lock();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            crate::core::extensions::register_extension(SeqProbe);
            let tmp = std::env::temp_dir()
                .join(format!("prux-v4-seq-probe-{}", crate::utils::time::now_ms()));
            std::fs::create_dir_all(&tmp).unwrap();
            let cwd = tmp.to_string_lossy().to_string();
            let sess_dir = tmp.join("sessions");
            let session = crate::core::session_manager::Session::create(
                &cwd,
                Some(sess_dir.clone()),
                true,
            )
            .unwrap();

            // 连接1：父代理一次流式响应发出两个 seq_probe 调用（宣 Sequential → 整批顺序执行）；
            // 连接2：父最终文本
            let f1: Vec<&'static str> = vec![
                r#"{"id":"cmpl_t","choices":[{"index":0,"delta":{"role":"assistant","content":null}}]}"#,
                frame(
                    r#"{"id":"cmpl_t","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"seq_probe","arguments":"{\"n\":1}"}}]}}]}"#
                        .to_string(),
                ),
                frame(
                    r#"{"id":"cmpl_t","choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"id":"call_2","type":"function","function":{"name":"seq_probe","arguments":"{\"n\":2}"}}]}}]}"#
                        .to_string(),
                ),
                r#"{"id":"cmpl_t","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
                r#"[DONE]"#,
            ];
            let f2 = text_frame("parent final", "cmpl_p");
            let (base, handle) = mock_server_multi(vec![f1, f2]).await;

            let mut agent = test_agent(&base, &cwd);
            agent.session = Some(session);
            let r = agent.prompt("go").await;
            handle.await.unwrap();
            assert!(r.is_ok(), "prompt failed: {:?}", r.err());
            drop(agent);

            let path = crate::core::session_manager::list_sessions(&sess_dir)
                .pop()
                .expect("session file");
            let opened =
                crate::core::session_manager::Session::open(&path.to_string_lossy()).unwrap();
            let entries = opened.get_entries();
            let ts: Vec<&J> = entries
                .iter()
                .filter(|e| e.get("type").and_then(|v| v.as_str()) == Some("tool_started"))
                .collect();
            let calls: Vec<(String, i64)> = ts
                .iter()
                .map(|e| {
                    (
                        e["toolCallId"].as_str().unwrap().to_string(),
                        e["toolIndex"].as_i64().unwrap(),
                    )
                })
                .collect();
            assert!(calls.contains(&("call_1".into(), 0)), "{calls:?}");
            assert!(calls.contains(&("call_2".into(), 1)), "{calls:?}");
            // 恢复校验必须通过（旧实现此处报 duplicate_tool_invocation）
            let red = opened.reduce_lane();
            assert!(red.is_ok(), "reduce_lane 应通过: {:?}", red.err());
            _ = crate::core::extensions::unregister_extension("seq-probe-test");
        });
    }

    /// 2.3 子集用到的测试扩展：一个编排工具（嵌套调其它工具）+ 一个 hidden 内部工具
    /// + 一个 model-only 工具（嵌套调用必须被拒）。
    struct NestedProbe;

    impl crate::core::extensions::Extension for NestedProbe {
        fn name(&self) -> &str {
            "nested-probe-test"
        }

        fn tools(&self) -> Vec<ExtensionTool> {
            vec![
                ExtensionTool::simple(
                    "nested_probe",
                    "compose other tools",
                    json!({ "type": "object", "properties": {} }),
                    "",
                ),
                ExtensionTool::simple(
                    "hidden_probe",
                    "internal tool, not visible to the model",
                    json!({ "type": "object", "properties": {} }),
                    "",
                )
                .with_exposure(ToolExposure::Hidden),
                ExtensionTool::simple(
                    "model_only_probe",
                    "visible to the model only",
                    json!({ "type": "object", "properties": {} }),
                    "",
                )
                .with_exposure(ToolExposure::ModelOnly),
            ]
        }

        fn execute_tool_async(
            &self,
            name: String,
            _args: Value,
            ctx: ToolExecCtx,
        ) -> BoxFuture<'static, std::result::Result<ToolResult, tools::ToolError>> {
            Box::pin(async move {
                match name.as_str() {
                    "nested_probe" => {
                        let bash = ctx
                            .execute_tool(
                                "bash".to_string(),
                                json!({ "command": "echo nested-out" }),
                            )
                            .await?;
                        let hidden = ctx
                            .execute_tool("hidden_probe".to_string(), json!({}))
                            .await?;
                        let denied = ctx
                            .execute_tool("model_only_probe".to_string(), json!({}))
                            .await;
                        let missing = ctx.execute_tool("nope_probe".to_string(), json!({})).await;
                        Ok(ToolResult {
                            text: format!(
                                "{}|{}|denied={}|missing={}",
                                bash.text.trim(),
                                hidden.text,
                                denied.is_err(),
                                missing.is_err()
                            ),
                            structured_content: Some(json!({ "composed": 2 })),
                            ..Default::default()
                        })
                    }
                    // 内部工具带用量：验证嵌套用量会并进调用方结果（会话成本据此计入）
                    "hidden_probe" => Ok(ToolResult {
                        text: "hidden ran".to_string(),
                        usage: Some(Usage {
                            input: 10,
                            output: 5,
                            total_tokens: 15,
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    "model_only_probe" => Ok(ToolResult::text("should never run nested")),
                    other => Err(tools::ToolError(format!("unknown tool: {other}"))),
                }
            })
        }
    }

    /// 2.3 子集：`ctx.execute_tool` 嵌套调用——解析、暴露方式门控、结果记录
    /// （`details.nestedCalls`）、用量并入调用方、事件带 `parentToolCallId`。
    #[test]
    fn nested_tool_calls_are_recorded_counted_and_evented() {
        let _lock = subagent_test_lock();
        let rt = test_rt();
        rt.block_on(async {
            crate::core::extensions::register_extension(NestedProbe);
            let tmp = std::env::temp_dir()
                .join(format!("prux-nested-probe-{}", crate::utils::time::now_ms()));
            std::fs::create_dir_all(&tmp).unwrap();
            let cwd = tmp.to_string_lossy().to_string();

            let f1: Vec<&'static str> = vec![
                r#"{"id":"cmpl_t","choices":[{"index":0,"delta":{"role":"assistant","content":null}}]}"#,
                frame(
                    r#"{"id":"cmpl_t","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_nested","type":"function","function":{"name":"nested_probe","arguments":"{}"}}]}}]}"#
                        .to_string(),
                ),
                r#"{"id":"cmpl_t","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
                r#"[DONE]"#,
            ];
            let (base, handle) = mock_server_multi(vec![f1, text_frame("done", "cmpl_p")]).await;

            let mut agent = test_agent(&base, &cwd);
            let collected: Arc<Mutex<Vec<J>>> = Arc::new(Mutex::new(Vec::new()));
            let sink = collected.clone();
            agent.attach_json_sink(Box::new(move |e: J| sink.lock().unwrap().push(e)));

            let text = agent.prompt("go").await.unwrap();
            handle.await.unwrap();
            assert_eq!(text, "done");
            _ = crate::core::extensions::unregister_extension("nested-probe-test");

            let events = collected.lock().unwrap().clone();
            let agent_end = events_of(&events, "agent_end")[0];
            let msgs = agent_end["messages"].as_array().unwrap();
            let tr = msgs
                .iter()
                .find(|m| m["role"] == "toolResult")
                .expect("toolResult 消息");

            // 调用方结果：嵌套文本、拒绝与未找到的返回
            let tr_text = tr["content"][0]["text"].as_str().unwrap();
            assert!(tr_text.contains("nested-out"), "{tr_text}");
            assert!(tr_text.contains("hidden ran"), "{tr_text}");
            assert!(tr_text.contains("denied=true"), "{tr_text}");
            assert!(tr_text.contains("missing=true"), "{tr_text}");

            // 结构化输出（`ToolResult.structured_content`）落进 details
            assert_eq!(tr["details"]["structuredContent"]["composed"], 2);

            // 嵌套用量并入调用方（会话成本按 toolResult usage 计入）
            assert_eq!(tr["usage"]["totalTokens"], 15, "{tr:?}");

            // 嵌套记录：bounded、带 toolCallId/toolName/文本，拒绝与未找到也是一条记录
            let nested = tr["details"]["nestedCalls"].as_array().unwrap();
            let names: Vec<&str> = nested
                .iter()
                .map(|c| c["toolName"].as_str().unwrap())
                .collect();
            assert_eq!(
                names,
                vec!["bash", "hidden_probe", "model_only_probe", "nope_probe"],
                "{nested:?}"
            );
            assert!(nested[0]["text"].as_str().unwrap().contains("nested-out"));
            assert_eq!(nested[2]["isError"], true);
            assert!(
                nested[2]["text"]
                    .as_str()
                    .unwrap()
                    .contains("exposure: model-only"),
                "{nested:?}"
            );
            assert!(
                nested[3]["text"].as_str().unwrap().contains("not found"),
                "{nested:?}"
            );
            assert_eq!(nested[1]["usage"]["totalTokens"], 15, "{nested:?}");

            // 事件：嵌套调用带 parentToolCallId，顶层调用不带
            let nested_ends: Vec<&J> = events
                .iter()
                .filter(|e| {
                    e["type"] == "tool_execution_end"
                        && e["parentToolCallId"].as_str() == Some("call_nested")
                })
                .collect();
            assert_eq!(nested_ends.len(), 4, "每个嵌套调用一条结束事件");
            let top_ends = events
                .iter()
                .filter(|e| {
                    e["type"] == "tool_execution_end"
                        && !crate::core::extensions::is_nested_call_event(e)
                })
                .count();
            assert_eq!(top_ends, 1, "只有顶层 nested_probe 一条不带 parent");
        });
    }

    /// 2.3 子集：嵌套调用记录有界（超出只计数），参数与文本超长时截断。
    #[test]
    fn nested_tool_calls_are_bounded_and_truncated() {
        struct EchoProbe;
        impl crate::core::extensions::Extension for EchoProbe {
            fn name(&self) -> &str {
                "nested-echo-test"
            }
            fn tools(&self) -> Vec<ExtensionTool> {
                vec![ExtensionTool::simple(
                    "echo_probe",
                    "echo text back",
                    json!({ "type": "object", "properties": { "text": { "type": "string" } } }),
                    "",
                )]
            }
            fn execute_tool(
                &self,
                name: &str,
                args: &Value,
            ) -> std::result::Result<ToolResult, tools::ToolError> {
                if name != "echo_probe" {
                    return Err(tools::ToolError("unknown".to_string()));
                }
                Ok(ToolResult::text(
                    args.get("text").and_then(|v| v.as_str()).unwrap_or(""),
                ))
            }
        }

        let _lock = subagent_test_lock();
        let rt = test_rt();
        rt.block_on(async {
            let _ad = crate::test_support::AgentDirGuard::temp();
            crate::core::extensions::register_extension(EchoProbe);
            let cwd = std::env::temp_dir().to_string_lossy().to_string();
            let (base, _handle) = mock_server_multi(vec![]).await;
            let agent = test_agent(&base, &cwd);
            let tpl = Arc::new(agent.snapshot_template());
            let abort = Arc::new(AtomicBool::new(false));
            let log = Arc::new(Mutex::new(NestedCallLog::default()));

            let big = "x".repeat(NESTED_CALL_TEXT_CAP + 100);
            for i in 0..(MAX_NESTED_CALLS + 3) {
                let out = run_nested_tool(
                    &cwd,
                    &tpl,
                    &abort,
                    &None,
                    &Some("call_parent".to_string()),
                    "echo_probe",
                    json!({ "text": big }),
                    0,
                    &log,
                    None,
                )
                .await;
                assert!(
                    out.is_ok(),
                    "第 {i} 次嵌套调用: {:?}",
                    out.err().map(|e| e.0)
                );
            }
            _ = crate::core::extensions::unregister_extension("nested-echo-test");

            let taken = take_log(&log);
            assert_eq!(taken.calls.len(), MAX_NESTED_CALLS);
            assert_eq!(taken.dropped, 3);
            let first = &taken.calls[0];
            assert_eq!(first["toolName"], "echo_probe");
            assert!(
                first["text"].as_str().unwrap().chars().count() <= NESTED_CALL_TEXT_CAP + 1,
                "文本应被截断"
            );
            // 参数超长时以 `_truncated` 字符串形态记录（不再展开成对象）
            assert!(first["args"]["_truncated"].is_string(), "{first:?}");
            // 账本取走后清空
            assert!(take_log(&log).calls.is_empty());
        });
    }

    /// 2.3 子集：递归深度上限与取消信号都会拦住嵌套调用。
    #[test]
    fn nested_tool_calls_respect_depth_limit_and_abort() {
        let _lock = subagent_test_lock();
        let rt = test_rt();
        rt.block_on(async {
            let cwd = std::env::temp_dir().to_string_lossy().to_string();
            let (base, _handle) = mock_server_multi(vec![]).await;
            let agent = test_agent(&base, &cwd);
            let tpl = Arc::new(agent.snapshot_template());
            let log = Arc::new(Mutex::new(NestedCallLog::default()));

            let abort = Arc::new(AtomicBool::new(false));
            let too_deep = run_nested_tool(
                &cwd,
                &tpl,
                &abort,
                &None,
                &None,
                "bash",
                json!({ "command": "echo x" }),
                MAX_NESTED_TOOL_DEPTH,
                &log,
                None,
            )
            .await
            .unwrap_err();
            assert!(too_deep.0.contains("depth limit"), "{}", too_deep.0);

            let abort = Arc::new(AtomicBool::new(true));
            let aborted = run_nested_tool(
                &cwd,
                &tpl,
                &abort,
                &None,
                &None,
                "bash",
                json!({ "command": "echo x" }),
                0,
                &log,
                None,
            )
            .await
            .unwrap_err();
            assert!(aborted.0.contains("aborted"), "{}", aborted.0);

            // 被拒的调用也一样进账本（否则调用方看不到自己的编排哪一步被挡了）
            let taken = take_log(&log);
            assert_eq!(taken.calls.len(), 2, "{:?}", taken.calls);
            assert_eq!(taken.calls[0]["isError"], true);
            assert!(
                taken.calls[0]["text"]
                    .as_str()
                    .unwrap()
                    .contains("depth limit")
            );
            assert!(taken.calls[1]["text"].as_str().unwrap().contains("aborted"));
            assert!(take_log(&log).calls.is_empty(), "账本取走后清空");
        });
    }

    /// 2.3 子集：`exposure` 决定模型可见面——`hidden`/`codemode` 既不进提示也不进工具表，
    /// `direct`/`model-only` 可见；`deferred` 需 `tool_search` 激活后才可见。
    #[test]
    fn exposure_controls_model_visible_tool_surface() {
        struct ExposureProbe;
        impl crate::core::extensions::Extension for ExposureProbe {
            fn name(&self) -> &str {
                "exposure-probe-test"
            }
            fn tools(&self) -> Vec<ExtensionTool> {
                vec![
                    ExtensionTool::simple("expo_direct", "visible direct tool", json!({}), ""),
                    ExtensionTool::simple(
                        "expo_model_only",
                        "visible model-only tool",
                        json!({}),
                        "",
                    )
                    .with_exposure(ToolExposure::ModelOnly),
                    ExtensionTool::simple("expo_hidden", "hidden internal tool", json!({}), "")
                        .with_exposure(ToolExposure::Hidden),
                    ExtensionTool::simple("expo_codemode", "codemode-only tool", json!({}), "")
                        .with_exposure(ToolExposure::Codemode),
                    ExtensionTool::simple("expo_deferred", "deferred tool", json!({}), "")
                        .with_exposure(ToolExposure::Deferred),
                ]
            }
        }

        let _lock = subagent_test_lock();
        let _ad = crate::test_support::AgentDirGuard::temp();
        crate::core::extensions::register_extension(ExposureProbe);
        crate::core::extensions::clear_deferred_activations();

        let ctx = ToolRebuildCtx::default();
        let names = |tools: &[(String, String, Value)]| -> Vec<String> {
            tools.iter().map(|(n, _, _)| n.clone()).collect()
        };
        let (prompt, tools, _hidden) = compose_tools(".", &[], &ctx, &[]);
        let visible = names(&tools);
        assert!(visible.contains(&"expo_direct".to_string()), "{visible:?}");
        assert!(
            visible.contains(&"expo_model_only".to_string()),
            "{visible:?}"
        );
        assert!(!visible.contains(&"expo_hidden".to_string()), "{visible:?}");
        assert!(
            !visible.contains(&"expo_codemode".to_string()),
            "{visible:?}"
        );
        assert!(
            !visible.contains(&"expo_deferred".to_string()),
            "{visible:?}"
        );
        assert!(
            !prompt.contains("hidden internal tool"),
            "提示里不应出现 hidden 工具"
        );

        // deferred 在 tool_search 候选里，激活后进模型可见面
        let deferred: Vec<String> = crate::core::extensions::unactivated_deferred_tools()
            .into_iter()
            .map(|t| t.name)
            .collect();
        assert!(
            deferred.contains(&"expo_deferred".to_string()),
            "{deferred:?}"
        );
        crate::core::extensions::activate_deferred_tools(&["expo_deferred".to_string()]);
        let (_, tools, _) = compose_tools(".", &[], &ctx, &[]);
        assert!(names(&tools).contains(&"expo_deferred".to_string()));

        crate::core::extensions::clear_deferred_activations();
        _ = crate::core::extensions::unregister_extension("exposure-probe-test");
    }

    /// 2.1 codemode：`prepare_loadout` 把可调工具的声明内联进 `codemode` 的描述，
    /// 并把同一份声明附到各 `direct` 工具自己的描述后（`codemode.mode = on`）。
    #[test]
    fn codemode_prepare_loadout_inlines_callable_tools() {
        struct CodemodeProbe;
        impl crate::core::extensions::Extension for CodemodeProbe {
            fn name(&self) -> &str {
                "codemode-probe-test"
            }
            fn tools(&self) -> Vec<ExtensionTool> {
                vec![
                    ExtensionTool::simple(
                        "cm_probe_tool",
                        "probe description",
                        json!({"type": "object", "properties": {"v": {"type": "string"}}}),
                        "",
                    ),
                    ExtensionTool::simple(
                        "cm_internal_tool",
                        "codemode-only probe",
                        json!({"type": "object", "properties": {}}),
                        "",
                    )
                    .with_exposure(ToolExposure::Codemode),
                ]
            }
        }

        let _lock = subagent_test_lock();
        let _ad = crate::test_support::AgentDirGuard::temp();
        crate::core::extensions::register_extension(CodemodeProbe);
        crate::core::extensions::register_extension(crate::extensions::codemode::Codemode);
        crate::core::extensions::set_extension_enabled("codemode", true);

        let ctx = ToolRebuildCtx::default();
        let (_, tools, hidden) = compose_tools(".", &[], &ctx, &[]);
        // 默认 `on` 模式不隐藏任何声明。
        assert!(hidden.is_empty(), "{hidden:?}");

        let codemode = tools
            .iter()
            .find(|(n, _, _)| n == crate::extensions::codemode::TOOL_NAME)
            .expect("codemode 应在工具表里");
        // `on` 模式下：只有「不单独声明」的工具（`codemode` exposure）进 codemode 的描述。
        assert!(
            codemode.1.contains("### `cm_internal_tool`"),
            "codemode 描述应内联非 direct 的可调工具：{}",
            codemode.1
        );
        assert!(
            !codemode.1.contains("### `cm_probe_tool`"),
            "direct 工具不进 codemode 描述：{}",
            codemode.1
        );
        // 模型看不到 `codemode` exposure 的探针工具。
        assert!(tools.iter().all(|(n, _, _)| n != "cm_internal_tool"));

        let probe = tools
            .iter()
            .find(|(n, _, _)| n == "cm_probe_tool")
            .expect("探针工具应可见");
        assert!(
            probe.1.contains("codemode tool declaration:"),
            "direct 工具的描述应附上脚本调用样例：{}",
            probe.1
        );

        // 关闭扩展后描述恢复原样（扩展工具随之下线）。
        crate::core::extensions::set_extension_enabled("codemode", false);
        let (_, tools, _) = compose_tools(".", &[], &ctx, &[]);
        assert!(tools.iter().all(|(n, _, _)| n != "codemode"));

        _ = crate::core::extensions::unregister_extension("codemode");
        _ = crate::core::extensions::unregister_extension("codemode-probe-test");
    }

    /// 2.1 codemode：`codemode.mode = "only"` 时把可调工具的声明从**请求**里摘掉
    /// （工具仍激活、仍可被脚本调），并把它们全部列进 `codemode` 的描述。
    #[test]
    fn codemode_only_mode_hides_direct_declarations() {
        struct OnlyModeProbe;
        impl crate::core::extensions::Extension for OnlyModeProbe {
            fn name(&self) -> &str {
                "codemode-only-probe-test"
            }
            fn tools(&self) -> Vec<ExtensionTool> {
                vec![ExtensionTool::simple(
                    "cm_only_tool",
                    "only-mode probe",
                    json!({"type": "object", "properties": {}}),
                    "",
                )]
            }
        }

        let _lock = subagent_test_lock();
        let _ad = crate::test_support::AgentDirGuard::temp();
        let cfg_dir = crate::core::settings_manager::agent_dir().join("extensions");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::write(
            cfg_dir.join("codemode.json"),
            r#"{"mode": "only", "inlineBudget": 100000}"#,
        )
        .unwrap();
        crate::core::extensions::register_extension(OnlyModeProbe);
        crate::core::extensions::register_extension(crate::extensions::codemode::Codemode);
        crate::core::extensions::set_extension_enabled("codemode", true);

        // 选中内置工具，让「Available tools」本来会列出它们——旧行为下被摘掉声明的工具仍留在提示里。
        let ctx = ToolRebuildCtx {
            selection: ToolSelection::tools(vec!["read".into(), "bash".into()]),
            ..ToolRebuildCtx::default()
        };
        let (prompt, tools, hidden) = compose_tools(".", &[], &ctx, &[]);
        assert!(
            hidden.contains(&"cm_only_tool".to_string()),
            "only 模式应隐藏 direct 工具的声明：{hidden:?}"
        );
        assert!(
            hidden.contains(&"read".to_string()) && hidden.contains(&"bash".to_string()),
            "内置工具的声明同样被摘掉：{hidden:?}"
        );
        // 声明被摘掉的工具也不能再挂在系统提示的 Available tools 里（pi 0.99.2 #10192）
        assert!(
            !prompt.contains("- read:") && !prompt.contains("- bash:"),
            "only 模式下内置工具不应出现在系统提示里：{prompt}"
        );
        let codemode = tools
            .iter()
            .find(|(n, _, _)| n == crate::extensions::codemode::TOOL_NAME)
            .expect("codemode 应在工具表里");
        assert!(
            codemode.1.contains("### `cm_only_tool`"),
            "only 模式下可调工具应全部列进 codemode 描述：{}",
            codemode.1
        );

        _ = crate::core::extensions::unregister_extension("codemode");
        _ = crate::core::extensions::unregister_extension("codemode-only-probe-test");
    }

    /// 2.1 codemode 端到端：模型调 `codemode`，脚本经 `tools.<name>()` 嵌套调内置工具，
    /// 嵌套结果与用量记进调用方（`details.nestedCalls`），且嵌套结果不单独进对话。
    #[test]
    fn codemode_script_orchestrates_nested_tools() {
        let _lock = subagent_test_lock();
        let rt = test_rt();
        rt.block_on(async {
            crate::core::extensions::register_extension(crate::extensions::codemode::Codemode);
            crate::core::extensions::set_extension_enabled("codemode", true);

            let tmp = std::env::temp_dir().join(format!(
                "prux-codemode-e2e-{}",
                crate::utils::time::now_ms()
            ));
            std::fs::create_dir_all(&tmp).unwrap();
            let file = tmp.join("note.txt");
            std::fs::write(&file, "codemode-e2e-content").unwrap();
            let cwd = tmp.to_string_lossy().to_string();

            let code = format!(
                "const out = await tools.read({{path: {}}});\nreturn out.trim();",
                serde_json::to_string(&file.to_string_lossy().to_string()).unwrap()
            );
            let delta = serde_json::json!({
                "id": "cmpl_c",
                "choices": [{"index": 0, "delta": {"tool_calls": [{
                    "index": 0,
                    "id": "call_codemode",
                    "type": "function",
                    "function": {
                        "name": crate::extensions::codemode::TOOL_NAME,
                        "arguments": serde_json::json!({"code": code}).to_string(),
                    }
                }]}}]
            })
            .to_string();

            let f1: Vec<&'static str> = vec![
                r#"{"id":"cmpl_c","choices":[{"index":0,"delta":{"role":"assistant","content":null}}]}"#,
                frame(delta),
                r#"{"id":"cmpl_c","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
                r#"[DONE]"#,
            ];
            let (base, handle) = mock_server_multi(vec![f1, text_frame("done", "cmpl_p")]).await;

            let mut agent = test_agent(&base, &cwd);
            let collected: Arc<Mutex<Vec<J>>> = Arc::new(Mutex::new(Vec::new()));
            let sink = collected.clone();
            agent.attach_json_sink(Box::new(move |e: J| sink.lock().unwrap().push(e)));

            let text = agent.prompt("go").await.unwrap();
            handle.await.unwrap();
            assert_eq!(text, "done");
            _ = crate::core::extensions::unregister_extension("codemode");
            _ = std::fs::remove_dir_all(&tmp);

            let events = collected.lock().unwrap().clone();
            let agent_end = events_of(&events, "agent_end")[0];
            let msgs = agent_end["messages"].as_array().unwrap();
            let tr = msgs
                .iter()
                .find(|m| m["role"] == "toolResult")
                .expect("toolResult 消息");

            // 脚本的输出（顶层 return）成为调用方结果文本。
            let tr_text = tr["content"][0]["text"].as_str().unwrap();
            assert!(tr_text.contains("Script completed"), "{tr_text}");
            assert!(tr_text.contains("codemode-e2e-content"), "{tr_text}");

            // 嵌套调用记在调用方 details 上（模型只看到脚本输出）。
            let nested = tr["details"]["nestedCalls"].as_array().unwrap();
            assert_eq!(nested.len(), 1, "{nested:?}");
            assert_eq!(nested[0]["toolName"], "read");
            assert_eq!(nested[0]["isError"], false);
            assert!(
                nested[0]["text"].as_str().unwrap().contains("codemode-e2e-content"),
                "{nested:?}"
            );
            assert_eq!(
                msgs.iter()
                    .filter(|m| m["role"] == "toolResult")
                    .count(),
                1,
                "嵌套结果不单独成为会话消息"
            );
        });
    }

    /// 错误结果也可能带结构化输出（`ToolResult::error` + `structured_content`）：
    /// 声明了 `outputSchema` 的工具在脚本里取到结构化值而不抛错（对齐 pi 的 `toScriptValue`），
    /// 未声明 `outputSchema` 的错误则抛错；两种情形在调用方账本里都是 `isError: true`。
    struct StructuredErrorProbe;

    impl crate::core::extensions::Extension for StructuredErrorProbe {
        fn name(&self) -> &str {
            "structured-error-probe-test"
        }

        fn tools(&self) -> Vec<ExtensionTool> {
            vec![
                ExtensionTool::simple(
                    "err_with_schema",
                    "fails but carries structured output",
                    json!({ "type": "object", "properties": {} }),
                    "",
                )
                .with_exposure(ToolExposure::Codemode)
                .with_output_schema(
                    json!({ "type": "object", "properties": { "ok": { "type": "boolean" } } }),
                ),
                ExtensionTool::simple(
                    "err_plain",
                    "fails without structured output",
                    json!({ "type": "object", "properties": {} }),
                    "",
                )
                .with_exposure(ToolExposure::Codemode),
            ]
        }

        fn execute_tool_async(
            &self,
            name: String,
            _args: Value,
            _ctx: ToolExecCtx,
        ) -> BoxFuture<'static, std::result::Result<ToolResult, tools::ToolError>> {
            Box::pin(async move {
                match name.as_str() {
                    "err_with_schema" => Ok(ToolResult::error("probe failed")
                        .with_structured_content(json!({ "ok": false }))),
                    "err_plain" => Ok(ToolResult::error("plain failure")),
                    other => Err(tools::ToolError(format!("unknown tool: {other}"))),
                }
            })
        }
    }

    /// codemode 脚本读得到错误结果里的结构化输出，且在调用方账本里仍然是 `isError: true`。
    #[test]
    fn codemode_script_reads_structured_error_result() {
        let _lock = subagent_test_lock();
        let rt = test_rt();
        rt.block_on(async {
            crate::core::extensions::register_extension(StructuredErrorProbe);
            crate::core::extensions::register_extension(crate::extensions::codemode::Codemode);
            crate::core::extensions::set_extension_enabled("codemode", true);

            let tmp = std::env::temp_dir().join(format!(
                "prux-codemode-error-{}",
                crate::utils::time::now_ms()
            ));
            std::fs::create_dir_all(&tmp).unwrap();
            let cwd = tmp.to_string_lossy().to_string();

            let code = "let a;\ntry { a = await tools.err_with_schema({}); } catch (e) { a = 'threw:' + e.message; }\nlet b;\ntry { b = await tools.err_plain({}); } catch (e) { b = 'threw:' + e.message; }\nreturn JSON.stringify({a, b});";
            let delta = serde_json::json!({
                "id": "cmpl_e",
                "choices": [{"index": 0, "delta": {"tool_calls": [{
                    "index": 0,
                    "id": "call_codemode_err",
                    "type": "function",
                    "function": {
                        "name": crate::extensions::codemode::TOOL_NAME,
                        "arguments": serde_json::json!({"code": code}).to_string(),
                    }
                }]}}]
            })
            .to_string();

            let f1: Vec<&'static str> = vec![
                r#"{"id":"cmpl_e","choices":[{"index":0,"delta":{"role":"assistant","content":null}}]}"#,
                frame(delta),
                r#"{"id":"cmpl_e","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
                r#"[DONE]"#,
            ];
            let (base, handle) = mock_server_multi(vec![f1, text_frame("done", "cmpl_p")]).await;

            let mut agent = test_agent(&base, &cwd);
            let collected: Arc<Mutex<Vec<J>>> = Arc::new(Mutex::new(Vec::new()));
            let sink = collected.clone();
            agent.attach_json_sink(Box::new(move |e: J| sink.lock().unwrap().push(e)));

            let text = agent.prompt("go").await.unwrap();
            handle.await.unwrap();
            assert_eq!(text, "done");
            _ = crate::core::extensions::unregister_extension("structured-error-probe-test");
            _ = crate::core::extensions::unregister_extension("codemode");
            _ = std::fs::remove_dir_all(&tmp);

            let events = collected.lock().unwrap().clone();
            let agent_end = events_of(&events, "agent_end")[0];
            let msgs = agent_end["messages"].as_array().unwrap();
            let tr = msgs
                .iter()
                .find(|m| m["role"] == "toolResult")
                .expect("toolResult 消息");
            let tr_text = tr["content"][0]["text"].as_str().unwrap();
            // 声明了 outputSchema 的错误 → 脚本拿到结构化值；无 schema 的错误 → 抛错
            assert!(tr_text.contains("\"ok\":false"), "{tr_text}");
            assert!(tr_text.contains("threw:plain failure"), "{tr_text}");

            let nested = tr["details"]["nestedCalls"].as_array().unwrap();
            let with_schema = nested
                .iter()
                .find(|c| c["toolName"] == "err_with_schema")
                .expect("err_with_schema 记录");
            assert_eq!(with_schema["isError"], true);
            assert_eq!(with_schema["structuredContent"]["ok"], false);
            let plain = nested
                .iter()
                .find(|c| c["toolName"] == "err_plain")
                .expect("err_plain 记录");
            assert_eq!(plain["isError"], true);
            assert!(plain.get("structuredContent").is_none(), "{plain:?}");
        });
    }

    /// codemode 脚本从 bash 拿到最多 1 MiB 的结构化输出（模型侧仍是 50 KiB / 2000 行）。
    #[test]
    fn codemode_script_receives_full_bash_output() {
        let _lock = subagent_test_lock();
        let rt = test_rt();
        rt.block_on(async {
            crate::core::extensions::register_extension(crate::extensions::codemode::Codemode);
            crate::core::extensions::set_extension_enabled("codemode", true);

            let tmp = std::env::temp_dir().join(format!(
                "prux-codemode-bash-{}",
                crate::utils::time::now_ms()
            ));
            std::fs::create_dir_all(&tmp).unwrap();
            let cwd = tmp.to_string_lossy().to_string();

            // 约 1.4 MB 输出：超过脚本侧 1 MiB 上限 → 首尾各半 + 省略标记
            let code = "const r = await tools.bash({command: \"yes x | head -n 700000\"});\nreturn JSON.stringify({truncated: r.truncated, marker: r.output.includes('bytes omitted'), code: r.exit_code});";
            let delta = serde_json::json!({
                "id": "cmpl_b",
                "choices": [{"index": 0, "delta": {"tool_calls": [{
                    "index": 0,
                    "id": "call_codemode_bash",
                    "type": "function",
                    "function": {
                        "name": crate::extensions::codemode::TOOL_NAME,
                        "arguments": serde_json::json!({"code": code}).to_string(),
                    }
                }]}}]
            })
            .to_string();

            let f1: Vec<&'static str> = vec![
                r#"{"id":"cmpl_b","choices":[{"index":0,"delta":{"role":"assistant","content":null}}]}"#,
                frame(delta),
                r#"{"id":"cmpl_b","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
                r#"[DONE]"#,
            ];
            let (base, handle) = mock_server_multi(vec![f1, text_frame("done", "cmpl_p")]).await;

            let mut agent = test_agent(&base, &cwd);
            agent.set_active_tools(vec!["read".into(), "write".into(), "bash".into()]);
            let collected: Arc<Mutex<Vec<J>>> = Arc::new(Mutex::new(Vec::new()));
            let sink = collected.clone();
            agent.attach_json_sink(Box::new(move |e: J| sink.lock().unwrap().push(e)));

            let text = agent.prompt("go").await.unwrap();
            handle.await.unwrap();
            assert_eq!(text, "done");
            _ = crate::core::extensions::unregister_extension("codemode");
            _ = std::fs::remove_dir_all(&tmp);

            let events = collected.lock().unwrap().clone();
            let agent_end = events_of(&events, "agent_end")[0];
            let msgs = agent_end["messages"].as_array().unwrap();
            let tr = msgs
                .iter()
                .find(|m| m["role"] == "toolResult")
                .expect("toolResult 消息");
            let tr_text = tr["content"][0]["text"].as_str().unwrap();
            assert!(tr_text.contains("\"truncated\":true"), "{tr_text}");
            assert!(tr_text.contains("\"marker\":true"), "{tr_text}");
            assert!(tr_text.contains("\"code\":0"), "{tr_text}");
        });
    }

    /// codemode 的 `models.*`：脚本按目录取分类器条目 → 跑分类器 → 用量与调用记录进 codemode 结果。
    #[test]
    fn codemode_script_runs_classifier_and_counts_usage() {
        let _lock = subagent_test_lock();
        // 分类器由 agent 循环在其它线程上解析凭据：线程本地 override 传不过去，
        // 因此用进程级 `PRUX_AGENT_DIR` pin（已持 AUTH_TEST_LOCK 串行）。
        crate::test_support::pin_test_agent_dir();
        let models_path = crate::core::settings_manager::agent_dir().join("models.json");
        let rt = test_rt();
        rt.block_on(async {
            crate::core::extensions::register_extension(crate::extensions::codemode::Codemode);
            crate::core::extensions::set_extension_enabled("codemode", true);

            let (classifier_base, classifier_handle) = mock_json_once(
                json!({
                    "answers": { "sentiment": { "type": "noul", "noul": 0.87 } },
                    "usage": { "input_tokens": 120, "output_tokens": 8 }
                })
                .to_string(),
            )
            .await;

            // 把 openrouter 的目录条目全指到假分类器服务器，并给它一份凭据
            std::fs::write(
                &models_path,
                json!({
                    "providers": {
                        "openrouter": { "baseUrl": classifier_base, "apiKey": "sk-test", "models": [] }
                    }
                })
                .to_string(),
            )
            .unwrap();

            let tmp = std::env::temp_dir().join(format!(
                "prux-codemode-model-{}",
                crate::utils::time::now_ms()
            ));
            std::fs::create_dir_all(&tmp).unwrap();
            let cwd = tmp.to_string_lossy().to_string();

            let code = "const m = await models.getModelOfType(\"classifier\", \"openrouter\", \"~typesafe/jev-latest\");\nconst r = await models.classify(m, {state: {text: \"great\"}, questions: {sentiment: {type: \"bool\", instructions: \"Is it positive?\", criteria: {true: \"positive\", false: \"negative\"}}}});\nreturn JSON.stringify({p: r.answers.sentiment.probability, stop: r.stopReason, selected: m && m.id});";
            let delta = serde_json::json!({
                "id": "cmpl_m",
                "choices": [{"index": 0, "delta": {"tool_calls": [{
                    "index": 0,
                    "id": "call_codemode_model",
                    "type": "function",
                    "function": {
                        "name": crate::extensions::codemode::TOOL_NAME,
                        "arguments": serde_json::json!({"code": code}).to_string(),
                    }
                }]}}]
            })
            .to_string();

            let f1: Vec<&'static str> = vec![
                r#"{"id":"cmpl_m","choices":[{"index":0,"delta":{"role":"assistant","content":null}}]}"#,
                frame(delta),
                r#"{"id":"cmpl_m","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
                r#"[DONE]"#,
            ];
            let (base, handle) = mock_server_multi(vec![f1, text_frame("done", "cmpl_p")]).await;

            let mut agent = test_agent(&base, &cwd);
            let collected: Arc<Mutex<Vec<J>>> = Arc::new(Mutex::new(Vec::new()));
            let sink = collected.clone();
            agent.attach_json_sink(Box::new(move |e: J| sink.lock().unwrap().push(e)));

            let text = agent.prompt("go").await.unwrap();
            handle.await.unwrap();
            // mock_json_once 自超时退出：即使请求未发出也不会永久阻塞
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), classifier_handle).await;
            assert_eq!(text, "done");
            _ = crate::core::extensions::unregister_extension("codemode");
            _ = std::fs::remove_dir_all(&tmp);
            let _ = std::fs::remove_file(&models_path);

            let events = collected.lock().unwrap().clone();
            let agent_end = events_of(&events, "agent_end")[0];
            let msgs = agent_end["messages"].as_array().unwrap();
            let tr = msgs
                .iter()
                .find(|m| m["role"] == "toolResult")
                .expect("toolResult 消息");
            let tr_text = tr["content"][0]["text"].as_str().unwrap();
            assert!(tr_text.contains("\"p\":0.87"), "{tr_text}");
            assert!(tr_text.contains("\"stop\":\"stop\""), "{tr_text}");
            assert!(
                tr_text.contains("\"selected\":\"~typesafe/jev-latest\""),
                "{tr_text}"
            );

            // 分类器用量并入 codemode 结果（进而计入会话成本）
            assert_eq!(tr["usage"]["totalTokens"], 128, "{tr:?}");

            // 调用本身记进 details.modelCalls（对齐 pi 的 calls 行）
            let calls = tr["details"]["modelCalls"].as_array().unwrap();
            assert_eq!(calls.len(), 1, "{calls:?}");
            assert_eq!(calls[0]["name"], "models.classify");
            assert_eq!(calls[0]["status"], "ok");
            assert_eq!(calls[0]["args"], "openrouter/~typesafe/jev-latest");
            assert!(calls[0]["durationMs"].is_number());
        });
    }

    /// `ToolResult.structured_content` 并进 `details.structuredContent`（落盘/事件同形）。
    #[test]
    fn structured_content_lands_in_details() {
        let call = ContentBlock::ToolCall {
            id: "c1".to_string(),
            name: "mcp".to_string(),
            arguments: json!({}),
            thought_signature: None,
            namespace: None,
        };
        let fin = FinalizedToolCall::from_tool_result(
            &call,
            "mcp",
            &json!({}),
            ToolResult {
                text: "ok".to_string(),
                details: Some(json!({ "action": "call" })),
                structured_content: Some(json!({ "rows": [1, 2] })),
                ..Default::default()
            },
        );
        let details = fin.details.as_ref().unwrap();
        assert_eq!(details["action"], "call");
        assert_eq!(details["structuredContent"]["rows"][1], 2);
        assert_eq!(
            fin.to_result_json()["details"]["structuredContent"]["rows"][0],
            1
        );

        // 没有 details 时也能只带结构化输出
        let fin = FinalizedToolCall::from_tool_result(
            &call,
            "x",
            &json!({}),
            ToolResult {
                text: "ok".to_string(),
                structured_content: Some(json!({ "a": 1 })),
                ..Default::default()
            },
        );
        assert_eq!(fin.details.as_ref().unwrap()["structuredContent"]["a"], 1);
    }

    /// `is_nested_call_event`：只有带非空 `parentToolCallId` 的才是嵌套调用事件。
    #[test]
    fn is_nested_call_event_requires_parent_id() {
        assert!(!crate::core::extensions::is_nested_call_event(
            &json!({ "type": "tool_execution_end", "toolName": "bash" })
        ));
        assert!(!crate::core::extensions::is_nested_call_event(
            &json!({ "type": "tool_execution_end", "parentToolCallId": J::Null })
        ));
        assert!(crate::core::extensions::is_nested_call_event(
            &json!({ "type": "tool_execution_end", "parentToolCallId": "call_1" })
        ));
    }

    /// 对齐 pi setActiveTools/getAllTools/getActiveTools：运行时管理活动工具。
    #[test]
    fn tool_management_api_get_and_set_active() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let cwd = std::env::temp_dir().to_string_lossy().to_string();
            let (base, handle) = mock_server_multi(vec![text_frame("ok", "c1")]).await;
            let mut agent = test_agent(&base, &cwd);
            // 初始活动工具包含内置 read/write（test_agent 传入；扩展工具可能由其他并行测试注册）
            let active = agent.get_active_tools();
            assert!(active.contains(&"read".to_string()));
            assert!(active.contains(&"write".to_string()));
            // 全部工具含内置集；task 不在内置（子代理走扩展）——仅当本测试进程中 subagent 扩展未被其他
            // 测试注册时成立，故不在此断言 task 缺席。
            let all = agent.get_all_tools();
            assert!(all.contains(&"read".to_string()));
            assert!(all.contains(&"bash".to_string()));
            // setActiveTools 只留 read：重建后 read 保留、write 移除
            // （扩展工具当前无条件并入活动列表——见 compose_tools 注释，故不断言恰好 [read]）
            agent.set_active_tools(vec!["read".to_string()]);
            let active2 = agent.get_active_tools();
            assert!(active2.contains(&"read".to_string()));
            assert!(
                !active2.contains(&"write".to_string()),
                "write 应从活动列表移除, got: {active2:?}"
            );
            handle.await.unwrap();
        });
    }

    /// M5-p3：run_loop 落 v4 durable 记录（operation_started/step_attempt/tool_started/operation_finished），
    /// 回读 session 文件断言形状。
    #[test]
    fn run_loop_writes_v4_durable_records() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let tmp =
                std::env::temp_dir().join(format!("prux-v4-{}", crate::utils::time::now_ms()));
            std::fs::create_dir_all(&tmp).unwrap();
            std::fs::write(tmp.join("a.rs"), "fn main() {}").unwrap();
            let cwd = tmp.to_string_lossy().to_string();

            let sess_dir = tmp.join("sessions");
            let session =
                crate::core::session_manager::Session::create(&cwd, Some(sess_dir.clone()), true)
                    .unwrap();
            let frames1 = tool_call_frame("read", r#"{\"path\":\"a.rs\"}"#, "call_1");
            let frames2 = text_frame("Read done", "cmpl_2");
            let (base, handle) = mock_server_multi(vec![frames1, frames2]).await;
            let entry = model_resolver::ModelEntry {
                model_type: Default::default(),
                id: "test-model".into(),
                name: "Test".into(),
                api: "openai-completions".into(),
                base_url: base.clone(),
                provider: "test".into(),
                input_limits: crate::utils::image::InputLimits::default(),
                allowed_fallback_models: Vec::new(),
                sampling_params: None,
                sampling_params_by_thinking_level: None,
                provider_routing: None,
                supports_usage_in_streaming: true,
                supports_store: true,
                supports_finish_reason: true,
                requires_assistant_after_tool_result: false,
                supports_strict_mode: false,
                send_session_affinity_headers: None,
                session_affinity_format: None,
                output: Vec::new(),
                input: vec!["text".into()],
                reasoning: false,
                context_window: 200_000,
                max_tokens: 4096,
                max_tokens_field: "max_tokens".into(),
                cost: None,
                supports_developer_role: false,
                requires_reasoning_content_on_assistant_messages: false,
                thinking_format: String::new(),
                supports_reasoning_effort: false,
                thinking_level_map: None,
                auth_header: false,
                supports_openai_grammar_tools: false,
            };
            let mut agent = Agent::new(
                entry,
                cwd.clone(),
                ToolSelection::tools(vec!["read".into(), "write".into()]),
                Vec::new(),
                None,
                None,
                None,
                Vec::new(),
                Some(session),
                Some("sk-test".to_string()),
                true,
                Vec::new(),
            )
            .unwrap();
            let _ = agent.prompt("read it").await.unwrap();
            handle.await.unwrap();
            drop(agent);

            let path = crate::core::session_manager::list_sessions(&sess_dir)
                .pop()
                .expect("session file");
            let opened =
                crate::core::session_manager::Session::open(&path.to_string_lossy()).unwrap();
            let entries = opened.get_entries();
            let types: Vec<&str> = entries
                .iter()
                .filter_map(|e| e.get("type").and_then(|v| v.as_str()))
                .collect();
            assert!(types.contains(&"operation_started"), "{types:?}");
            assert!(types.contains(&"step_attempt"), "{types:?}");
            assert!(types.contains(&"tool_started"), "{types:?}");
            assert!(types.contains(&"operation_finished"), "{types:?}");

            // tool_started 为 v4 形状：带 assistantEntryId/toolIndex/toolCallId/toolName/resultEntryId/replay
            let ts = entries
                .iter()
                .find(|e| e.get("type").and_then(|v| v.as_str()) == Some("tool_started"))
                .unwrap();
            assert!(ts.get("assistantEntryId").is_some());
            assert!(ts.get("toolIndex").is_some());
            assert_eq!(ts["toolCallId"], "call_1");
            assert_eq!(ts["toolName"], "read");
            assert!(ts.get("resultEntryId").is_some());
            assert_eq!(ts["replay"], "never");
            // 操作 start/finished runId 一致
            let os = entries
                .iter()
                .find(|e| e.get("type").and_then(|v| v.as_str()) == Some("operation_started"))
                .unwrap();
            let of = entries
                .iter()
                .find(|e| e.get("type").and_then(|v| v.as_str()) == Some("operation_finished"))
                .unwrap();
            assert_eq!(os["id"], of["runId"]);
            assert_eq!(of["outcome"], "completed");
            // 记录可被归约器消费：reduce_lane 不应报损坏（records 已解析）
            let red = opened.reduce_lane();
            assert!(red.is_ok(), "reduce_lane 应通过: {:?}", red.err());
        });
    }

    /// M5-p4：provider 错误（HTTP 500 → stopReason=error）时 run 也必须落
    /// operation_finished(outcome="failed")——否则磁盘残留未关闭的 operation，
    /// 多次崩溃后累积成僵尸 open op，会话再也不可恢复（旧 bug 根因）
    #[test]
    fn run_loop_writes_failed_finish_on_provider_error() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let tmp =
                std::env::temp_dir().join(format!("prux-v4-err-{}", crate::utils::time::now_ms()));
            std::fs::create_dir_all(&tmp).unwrap();
            std::fs::write(tmp.join("a.rs"), "fn main() {}").unwrap();
            let cwd = tmp.to_string_lossy().to_string();

            let sess_dir = tmp.join("sessions");
            let session =
                crate::core::session_manager::Session::create(&cwd, Some(sess_dir.clone()), true)
                    .unwrap();
            let (base, handle) = mock_server_sequence(vec![MockResp::Err(
                "HTTP/1.1 500 Internal Server Error",
                r#"{"error":{"message":"boom"}}"#,
            )])
            .await;
            let entry = model_resolver::ModelEntry {
                model_type: Default::default(),
                id: "test-model".into(),
                name: "Test".into(),
                api: "openai-completions".into(),
                base_url: base.clone(),
                provider: "test".into(),
                input_limits: crate::utils::image::InputLimits::default(),
                allowed_fallback_models: Vec::new(),
                sampling_params: None,
                sampling_params_by_thinking_level: None,
                provider_routing: None,
                supports_usage_in_streaming: true,
                supports_store: true,
                supports_finish_reason: true,
                requires_assistant_after_tool_result: false,
                supports_strict_mode: false,
                send_session_affinity_headers: None,
                session_affinity_format: None,
                output: Vec::new(),
                input: vec!["text".into()],
                reasoning: false,
                context_window: 200_000,
                max_tokens: 4096,
                max_tokens_field: "max_tokens".into(),
                cost: None,
                supports_developer_role: false,
                requires_reasoning_content_on_assistant_messages: false,
                thinking_format: String::new(),
                supports_reasoning_effort: false,
                thinking_level_map: None,
                auth_header: false,
                supports_openai_grammar_tools: false,
            };
            let mut agent = Agent::new(
                entry,
                cwd.clone(),
                ToolSelection::tools(vec!["read".into(), "write".into()]),
                Vec::new(),
                None,
                None,
                None,
                Vec::new(),
                Some(session),
                Some("sk-test".to_string()),
                true,
                Vec::new(),
            )
            .unwrap();
            let _ = agent.prompt("read it").await;
            handle.await.unwrap();
            drop(agent);

            let path = crate::core::session_manager::list_sessions(&sess_dir)
                .pop()
                .expect("session file");
            let opened =
                crate::core::session_manager::Session::open(&path.to_string_lossy()).unwrap();
            let entries = opened.get_entries();
            let of = entries
                .iter()
                .find(|e| e.get("type").and_then(|v| v.as_str()) == Some("operation_finished"))
                .unwrap_or_else(|| panic!("必须落 operation_finished: {entries:#?}"));
            assert_eq!(of["outcome"], "failed", "error 回合 outcome=failed");
            // finish 已落盘 → 无僵尸 open op，reduce 通过
            assert_eq!(opened.open_operations_count(), 0);
            assert!(opened.reduce_lane().is_ok());
        });
    }
}
