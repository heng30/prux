//! `memory` 扩展：跨会话仍然成立的记忆——自动抽取、按相关性召回、模型可显式读写。
//!
//! 三件事：
//!
//! 1. **抽取**：每 `extractInterval` 次 `turn_end` 之后（以及每轮结算时）用**一次**独立的一次性
//!    请求，把最近一段对话摘录抽成结构化条目（事实 / 偏好 / 实体 / 纠正），写进记忆图；
//! 2. **召回**：每次请求前按当前用户消息做**词法** BM25 召回（prux 没有 embedding 栈），
//!    把命中条目拼成一段稳定文本追加进本次请求的上下文。它**不进会话历史**，也不改系统提示；
//! 3. **工具**：给模型一个 `memory` 工具（`remember` / `recall` / `search` / `list` /
//!    `forget` / `tag` / `link` / `related`），让用户可以显式要求「记住这件事」或「忘掉它」。
//!
//! 与 `subagent` 的 per-agent `MEMORY.md` 的分工：那是**用户手写**、整份注入的说明文件；
//! 这里是 agent **自动抽取 + 按相关性挑选**、带条目级治理（置信度、取代、忘记）的记忆图。
//!
//! 存储：`agent_dir()/extensions/memory/global.json` 与
//! `agent_dir()/extensions/memory/projects/<project-key>.json`（见 [`store`]）。
//! **记忆落盘在本机**，不会有网络请求——只有抽取那一步会把摘录发给当前模型。
//!
//! 注入块**必须稳定**：同样的图 + 同样的查询必得同样的文本，且一轮之内只算一次，否则会把
//! prompt cache 打掉。因此召回结果按「分数 → 更新时间 → id」确定性排序，并缓存在进程状态里。
//!
//! 默认**不启用**（`Dev` / `Creator`）：它会注入提示词、花额外模型请求，不该由框架替用户决定。

mod config;
mod extract;
mod graph;
mod recall;
mod store;

use crate::{
    core::{
        auth,
        extensions::{
            self, BoundaryOutcome, Extension, ExtensionCommand, ExtensionHook, ExtensionMode,
            ExtensionSetting, ExtensionTool, ExtensionUiRequest, ForkProjectInfo, SubcommandDef,
            ToolAnnotations, ToolExecCtx, ToolExposure,
        },
        model_resolver,
        provider::{self, AgentMessage},
        runtime,
        session_manager::Session,
        tools::{ToolError, ToolResult},
    },
    extensions::{
        EXTENSION_FACTORIES, ExtensionFactory, PRIORITY_MEMORY, command_arg,
        util::{push_context, session_id_of, split_model},
    },
    modes::interactive::{
        app::{App, MsgLevel},
        handlers::register_slash_command,
    },
    utils::{
        glyphs::DEF_ELLIPSIS,
        time::{now_iso, short_date},
        truncate::truncate_chars,
    },
};
use config::{Config, EXT, merge_config};
use extract::ExtractedMemory;
use futures_util::future::BoxFuture;
use graph::{MemoryCategory, MemoryEntry, MemoryGraph, MemoryScope};
use recall::Ranked;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{Arc, Mutex, MutexGuard, OnceLock},
};
use store::{Located, Scoped};

/// 模型可见的工具名。
const TOOL_MEMORY: &str = "memory";

/// `/memory` 的子命令（顺序 = 输入框候选顺序）。
const SUBCOMMANDS: &[SubcommandDef] = &[
    SubcommandDef {
        name: "status",
        description: "Show memory counts, storage path and current settings (default)",
    },
    SubcommandDef {
        name: "list",
        description: "List stored memories: /memory list [project|global|all]",
    },
    SubcommandDef {
        name: "search",
        description: "Search memories by keyword: /memory search <query>",
    },
    SubcommandDef {
        name: "forget",
        description: "Delete one memory by id: /memory forget <id>",
    },
    SubcommandDef {
        name: "settings",
        description: "Open the memory settings panel",
    },
];

/// `recall` 的默认条数。
const DEFAULT_RECALL_LIMIT: usize = 10;
/// `recall` 的条数上界。
const MAX_RECALL_LIMIT: usize = 50;
/// `search` / `list` 的默认条数。
const DEFAULT_LIST_LIMIT: usize = 100;
/// `search` / `list` 的条数上界。
const MAX_LIST_LIMIT: usize = 500;
/// `related` 的默认跳数。
const DEFAULT_RELATED_DEPTH: usize = 2;
/// `related` 的跳数上界。
const MAX_RELATED_DEPTH: usize = 5;
/// 展示条目里正文的最大字符数（超出以省略号收尾）。
const CONTENT_PREVIEW_CHARS: usize = 240;
/// 抽取提示里「已知条目」单条的字符上限。
const KNOWN_PREVIEW_CHARS: usize = 160;
/// 召回用的查询文本上限（字符）：超长输入只取前段。
const QUERY_MAX_CHARS: usize = 2000;

/// 自声明工厂：linkme 分布式切片。
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static MEMORY_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_MEMORY,
    make: || -> Arc<dyn Extension> { Arc::new(Memory) },
};

/// 一次抽取的结果摘要（`/memory status` 展示）。
#[derive(Debug, Clone)]
struct ExtractNote {
    /// 完成时间（UTC RFC3339）。
    at: String,
    /// 新增条数。
    added: usize,
    /// 重复确认（强化）条数。
    reinforced: usize,
    /// 失败原因；成功为 `None`。
    error: Option<String>,
}

/// 扩展的进程级状态。
struct State {
    /// 运行期配置。
    cfg: Config,
    /// 当前工作目录（会话开始 / `on_exec_ctx` 时更新）。
    cwd: Option<String>,
    /// 当前会话 id（抽取条目的 `source`）。
    session_id: Option<String>,
    /// 当前型号 `(provider, model_id)`（抽取请求用）。
    model: Option<(String, String)>,
    /// 召回缓存：`(查询文本, 注入块)`；块为 `None` 表示「这一轮没有可注入的内容」。
    recall_cache: Option<(String, Option<String>)>,
    /// 距上次抽取累计的 `turn_end` 次数。
    turns_since_extract: usize,
    /// 是否已到抽取时机（等待结算时执行）。
    extract_pending: bool,
    /// 是否已有一个抽取请求在飞（避免叠加）。
    extracting: bool,
    /// 最近一次抽取结果。
    last_extract: Option<ExtractNote>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            cfg: merge_config(None),
            cwd: None,
            session_id: None,
            model: None,
            recall_cache: None,
            turns_since_extract: 0,
            extract_pending: false,
            extracting: false,
            last_extract: None,
        }
    }
}

/// 取扩展状态的进程级单例。
fn state() -> &'static Mutex<State> {
    /// 扩展状态的进程级单例。
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(State::default()))
}

/// 取状态锁；锁被毒化时取回内部值。
fn lock() -> MutexGuard<'static, State> {
    state().lock().unwrap_or_else(|e| e.into_inner())
}

/// `memory` 扩展。
pub struct Memory;

impl Extension for Memory {
    /// 扩展名 `memory`。
    fn name(&self) -> &str {
        EXT
    }

    /// 返回 `/extension` 面板展示的一句话描述。
    fn description(&self) -> &str {
        "Durable cross-session memory: auto-extract, relevance recall, and a memory tool."
    }

    /// 声明本扩展移植自 jcode 0.94.0，供 `/extension` 面板展示来源链接。
    fn fork_project(&self) -> Option<ForkProjectInfo> {
        Some(ForkProjectInfo {
            plugin_name: "jcode".to_string(),
            plugin_version: "0.94.0".to_string(),
            url: "https://github.com/1jehuang/jcode".to_string(),
        })
    }

    /// 默认关闭：会注入提示词并花额外模型请求。
    fn default_enabled(&self) -> bool {
        false
    }

    /// 只在 `Dev` / `Creator` 扩展模式下可用。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Dev, ExtensionMode::Creator]
    }

    /// 提供 `memory` 工具。
    fn tools(&self) -> Vec<ExtensionTool> {
        vec![memory_tool()]
    }

    /// 提供 `/memory` 命令。
    fn commands(&self) -> Vec<ExtensionCommand> {
        vec![ExtensionCommand {
            name: config::CMD.to_string(),
            description: "Inspect stored memory — status, list, search, forget, settings"
                .to_string(),
            busy_safe: true, // handler 只读写扩展状态与记忆文件，不锁 agent
            subcommands: SUBCOMMANDS.to_vec(),
        }]
    }

    /// 订阅系统提示注入与边界（抽取节奏）两类钩子。
    fn hooks(&self) -> Vec<ExtensionHook> {
        vec![ExtensionHook::ContextWithSystem, ExtensionHook::Boundary]
    }

    /// 面板设置项：召回条数、抽取间隔、注入预算。
    fn settings(&self) -> Vec<ExtensionSetting> {
        config::panel_settings(&lock().cfg)
    }

    /// 应用面板选择并立即落盘。
    fn apply_setting(&self, key: &str, value: &str) -> std::result::Result<(), String> {
        let cfg = {
            let mut st = lock();
            config::apply_setting(&mut st.cfg, key, value)?;
            st.recall_cache = None;
            st.cfg.clone()
        };
        config::write_config(&config::config_path(), &cfg);
        Ok(())
    }

    /// 接线 `/memory` 的 handler。
    fn on_registered(&self) {
        register_slash_command(EXT, config::CMD, command_memory);
    }

    /// 启用时加载配置；禁用时清掉运行期状态（配置保留）。
    fn on_enabled_changed(&self, enabled: bool) {
        let mut st = lock();
        if enabled {
            st.cfg = config::load_config();
        }
        st.recall_cache = None;
        st.turns_since_extract = 0;
        st.extract_pending = false;
        st.extracting = false;
    }

    /// 会话开始：记住 cwd / 会话 id，清掉上一会话的召回缓存与抽取节奏。
    fn on_session_start(&self, cwd: &str, _messages: &[AgentMessage], session: Option<&Session>) {
        let mut st = lock();
        st.cwd = Some(cwd.to_string());
        st.session_id = session.map(|s| s.session_id.clone());
        st.recall_cache = None;
        st.turns_since_extract = 0;
        st.extract_pending = false;
    }

    /// 会话切换：改记新会话 id 并复位召回缓存与节奏。
    fn on_session_switched(&self, session_path: Option<&str>, _messages: &[AgentMessage]) {
        let mut st = lock();
        st.session_id = session_id_of(session_path);
        st.recall_cache = None;
        st.turns_since_extract = 0;
        st.extract_pending = false;
    }

    /// 缓存当前型号与工作目录（抽取请求要用；子代理上下文不覆盖）。
    fn on_exec_ctx(&self, ctx: &ToolExecCtx) {
        if ctx.depth > 0 {
            return;
        }
        let mut st = lock();
        if !ctx.cwd.trim().is_empty() {
            st.cwd = Some(ctx.cwd.clone());
        }
        if let Some(model) = ctx.parent_model.as_deref().and_then(split_model) {
            st.model = Some(model);
        }
    }

    /// 请求前注入召回块：只在查询变化时重算，同一轮内复用缓存。
    ///
    /// 用 `ContextWithSystem` 而不是 `TransformContext`：前者结果原样发送，**不改写**
    /// `agent.messages`，所以注入不会进会话历史、也不会被后续压缩当成真实对话。
    fn transform_context_with_system(
        &self,
        messages: &mut Vec<AgentMessage>,
    ) -> crate::error::Result<()> {
        let Some(query) = last_user_query(messages) else {
            return Ok(());
        };

        let (cfg, cwd, cached) = {
            let st = lock();
            (st.cfg.clone(), st.cwd.clone(), st.recall_cache.clone())
        };

        let block = match cached {
            Some((cached_query, block)) if cached_query == query => block,
            _ => {
                let block = build_recall_block(&cfg, cwd.as_deref(), &query);
                lock().recall_cache = Some((query, block.clone()));
                block
            }
        };

        if let Some(block) = block {
            push_context(messages, &block);
        }
        Ok(())
    }

    /// 边界：`turn_end` 推进抽取 cadence；`agent_before_settle` 发起抽取请求。
    fn on_boundary(&self, event: &Value) -> Option<BoundaryOutcome> {
        if event.get("agentScope").and_then(Value::as_str) != Some("main") {
            return None;
        }
        match event.get("type").and_then(Value::as_str) {
            Some("turn_end") => {
                advance_cadence();
                None
            }
            Some("agent_before_settle") => {
                maybe_spawn_extraction(event);
                None
            }
            _ => None,
        }
    }

    /// 按工具名分发；未知工具名返回 `ToolError`。
    fn execute_tool_async(
        &self,
        name: String,
        args: Value,
        ctx: ToolExecCtx,
    ) -> BoxFuture<'static, Result<ToolResult, ToolError>> {
        Box::pin(async move {
            if name != TOOL_MEMORY {
                return Err(ToolError(format!("extension {EXT}: unknown tool: {name}")));
            }

            let session_id = lock().session_id.clone();
            let cwd = (!ctx.cwd.trim().is_empty()).then_some(ctx.cwd.clone());
            let result = run_action(&args, cwd.as_deref(), session_id.as_deref());

            // 让模型自己刚写/删的记忆在下一次请求就参与召回，而不是等下一轮才刷新
            if result.is_ok() {
                lock().recall_cache = None;
            }
            result
        })
    }
}

/// 构造 `memory` 工具定义。
///
/// 声明为**直接工具**（[`ToolExposure::Direct`]）：`remember` / `forget` 是 push 型动作，
/// 靠的是模型每轮都看得见的那条 guideline（「用户说出持久偏好时调 `remember`」），
/// 延迟到 `tool_search` 激活就会把这条驱动指令一起藏起来——空记忆冷启动时注入块也不会出现，
/// 模型手里没有任何「可以写记忆」的信号，`forget` 更是无从发现。
/// 它省下的 snippet + guidelines 只有四百余字符，比召回注入块小一个数量级，不值得。
fn memory_tool() -> ExtensionTool {
    ExtensionTool {
        exposure: ToolExposure::Direct,
        namespace: None,
        annotations: ToolAnnotations::default(),
        output_schema: None,
        grammar_sampling: None,
        name: TOOL_MEMORY.to_string(),
        description: "Store and recall durable notes that outlive this session: user preferences, \
                      corrections, project conventions, environment facts. Actions: remember, \
                      recall, search, list, forget, tag, link, related."
            .to_string(),
        label: Some("Memory".to_string()),
        parameters: json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["remember", "recall", "search", "list", "forget", "tag", "link", "related"],
                    "description": "What to do."
                },
                "content": {
                    "type": "string",
                    "description": "remember: the memory text. One self-contained sentence in the user's language."
                },
                "category": {
                    "type": "string",
                    "enum": ["fact", "preference", "entity", "correction"],
                    "description": "remember: memory category (default fact)."
                },
                "query": {
                    "type": "string",
                    "description": "recall/search: keywords. Prefer distinctive terms; Chinese is matched by character bigrams."
                },
                "id": {
                    "type": "string",
                    "description": "forget/tag/related: target memory id."
                },
                "tags": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "remember/tag: tags to attach. In `tag`, a leading `-` removes that tag (e.g. \"-tooling\")."
                },
                "scope": {
                    "type": "string",
                    "enum": ["project", "global", "all"],
                    "description": "project = this working directory, global = everywhere. remember defaults to project; recall/search/list default to all."
                },
                "supersedes": {
                    "type": "string",
                    "description": "remember: id of an older memory this one replaces. The old entry stays on disk but stops being recalled."
                },
                "from_id": { "type": "string", "description": "link: source memory id." },
                "to_id": { "type": "string", "description": "link: target memory id." },
                "weight": {
                    "type": "number",
                    "minimum": 0.0,
                    "maximum": 1.0,
                    "description": "link: association strength (default 0.5)."
                },
                "depth": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_RELATED_DEPTH,
                    "description": format!("related: traversal depth (default {DEFAULT_RELATED_DEPTH}).")
                },
                "limit": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": MAX_LIST_LIMIT,
                    "description": format!("recall/search/list: max results. recall defaults to {DEFAULT_RECALL_LIMIT}, search/list to {DEFAULT_LIST_LIMIT}; 0 returns nothing.")
                }
            },
            "required": ["action"]
        }),
        snippet: "Use `memory` to persist durable facts and preferences across sessions, and to \
                  look up what was remembered earlier."
            .to_string(),
        prompt_guidelines: vec![
            "Call `memory` with `remember` when the user states a durable preference, a correction, \
             or a project convention that should still hold next session. Use `scope: \"global\"` \
             for user-wide preferences and the default `project` for repo specifics."
                .to_string(),
            "`recall` / `search` are lexical: pass distinctive keywords, not full sentences. Use \
             `list` to discover ids and `forget` when the user asks you to forget something."
                .to_string(),
        ],
        constrained_sampling: false,
        render_shell: None,
        execution_mode: None,
        prepare_arguments: None,
    }
}

/// `memory` 工具的参数。
#[derive(Debug, Default, Deserialize)]
struct Params {
    /// 动作名（见工具的 `action` 枚举）。
    #[serde(default)]
    action: String,
    /// `remember` 的正文。
    #[serde(default)]
    content: Option<String>,
    /// `remember` 的分类。
    #[serde(default)]
    category: Option<String>,
    /// `recall` / `search` 的关键词。
    #[serde(default)]
    query: Option<String>,
    /// `forget` / `tag` / `related` 的目标 id。
    #[serde(default)]
    id: Option<String>,
    /// `remember` / `tag` 的标签。
    #[serde(default)]
    tags: Option<Vec<String>>,
    /// 作用域。
    #[serde(default)]
    scope: Option<String>,
    /// `remember` 要取代的旧条目 id。
    #[serde(default)]
    supersedes: Option<String>,
    /// `link` 的源 id。
    #[serde(default)]
    from_id: Option<String>,
    /// `link` 的目标 id。
    #[serde(default)]
    to_id: Option<String>,
    /// `link` 的关联权重。
    #[serde(default)]
    weight: Option<f32>,
    /// `related` 的跳数。
    #[serde(default)]
    depth: Option<usize>,
    /// 结果条数上限。
    #[serde(default)]
    limit: Option<usize>,
}

/// 执行 `memory` 工具的一个动作。
///
/// 与扩展解耦的纯入口（只依赖 `cwd` / `session_id` 与磁盘），便于单测直接驱动。
fn run_action(
    args: &Value,
    cwd: Option<&str>,
    session_id: Option<&str>,
) -> Result<ToolResult, ToolError> {
    let params: Params = serde_json::from_value(args.clone())
        .map_err(|err| ToolError(format!("invalid `memory` arguments: {err}")))?;
    let action = params.action.trim();

    match action {
        "remember" => action_remember(&params, cwd, session_id),
        "recall" => action_recall(&params, cwd),
        "search" => action_search(&params, cwd),
        "list" => action_list(&params, cwd),
        "forget" => action_forget(&params, cwd),
        "tag" => action_tag(&params, cwd),
        "link" => action_link(&params, cwd),
        "related" => action_related(&params, cwd),
        other => Err(ToolError(format!(
            "unknown `memory` action {other:?}: expected remember, recall, search, list, forget, tag, link or related"
        ))),
    }
}

/// `remember`：写一条记忆（同分类同正文视为「已知道」→ 强化）。
fn action_remember(
    params: &Params,
    cwd: Option<&str>,
    session_id: Option<&str>,
) -> Result<ToolResult, ToolError> {
    let content = params
        .content
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| ToolError("`content` is required for `remember`.".to_string()))?;
    let category = MemoryCategory::parse_lenient(params.category.as_deref().unwrap_or("fact"));
    let scope = parse_scope(params.scope.as_deref(), MemoryScope::Project)?;
    if scope == MemoryScope::All {
        return Err(ToolError(
            "`remember` needs a concrete scope: \"project\" or \"global\".".to_string(),
        ));
    }
    let tags = normalize_tags(params.tags.as_deref());

    let mut located = match scope {
        MemoryScope::Global => store::read_global(),
        _ => store::read_project(cwd.unwrap_or("")),
    }
    .map_err(ToolError)?;

    let duplicate = located
        .graph
        .active_memories()
        .find(|entry| entry.category == category && entry.content.trim() == content)
        .map(|entry| entry.id.clone());

    if let Some(id) = duplicate {
        if let Some(entry) = located.graph.get_mut(&id) {
            entry.reinforce(session_id.unwrap_or("memory-tool"));
        }
        located.save().map_err(|err| ToolError(format!("{err}")))?;
        let strength = located.graph.get(&id).map(|e| e.strength).unwrap_or(1);
        return Ok(ToolResult::text(format!(
            "Already known ({} / {}): \"{}\"\nid: {}  strength: {}",
            category.as_label(),
            scope.as_label(),
            content,
            id,
            strength
        )));
    }

    let supersedes = params
        .supersedes
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty());
    if let Some(old) = supersedes
        && located.graph.get(old).is_none()
    {
        return Err(ToolError(format!(
            "cannot supersede {old}: not found in the {} store",
            scope.as_label()
        )));
    }

    let entry = MemoryEntry::new(category, content)
        .with_source(session_id.unwrap_or("memory-tool"))
        .with_tags(tags);
    let id = located.graph.add_memory(entry);
    if let Some(old) = supersedes {
        located.graph.supersede(&id, old);
    }
    located.save().map_err(|err| ToolError(format!("{err}")))?;

    let replaced = match supersedes {
        Some(old) => format!("\nsuperseded: {old}"),
        None => String::new(),
    };
    Ok(ToolResult::text(format!(
        "Remembered ({} / {}): \"{}\"\nid: {}{replaced}",
        category.as_label(),
        scope.as_label(),
        content,
        id
    )))
}

/// `recall`：按查询做 BM25 召回；无查询时给「最近/最重要」的条目。
fn action_recall(params: &Params, cwd: Option<&str>) -> Result<ToolResult, ToolError> {
    let scope = parse_scope(params.scope.as_deref(), MemoryScope::All)?;
    let limit = params
        .limit
        .unwrap_or(DEFAULT_RECALL_LIMIT)
        .min(MAX_RECALL_LIMIT);
    if limit == 0 {
        return Ok(ToolResult::text("No memories recalled (limit 0)."));
    }

    let mut scoped = store::read_scoped(scope, cwd).map_err(ToolError)?;
    let active = scoped.active_entries();
    let query = params
        .query
        .as_deref()
        .map(str::trim)
        .filter(|q| !q.is_empty());

    let ranked = match query {
        Some(query) => {
            if recall::query_terms(query).is_empty() {
                return Err(ToolError(format!(
                    "`query` has no searchable keyword ({query:?}); use a more distinctive term."
                )));
            }
            recall::rank(&active, query, limit)
        }
        None => recall::recent(&active, limit),
    };
    touch_hits(&mut scoped, &ranked);

    if ranked.is_empty() {
        let suffix = match query {
            Some(query) => format!(" matching {query:?}"),
            None => String::new(),
        };
        return Ok(ToolResult::text(format!(
            "No memories found{suffix}. Store one with action \"remember\"."
        )));
    }

    let mut out = match query {
        Some(query) => format!("Recalled {} memories for {query:?}:\n\n", ranked.len()),
        None => format!("Most relevant {} memories:\n\n", ranked.len()),
    };
    for item in &ranked {
        out.push_str(&render_entry(item.scope, &item.entry));
    }
    Ok(ToolResult::text(out))
}

/// `search`：列出命中任意查询词的全部条目（按 BM25 排序，可截断）。
fn action_search(params: &Params, cwd: Option<&str>) -> Result<ToolResult, ToolError> {
    let query = params
        .query
        .as_deref()
        .map(str::trim)
        .filter(|q| !q.is_empty())
        .ok_or_else(|| ToolError("`query` is required for `search`.".to_string()))?;
    if recall::query_terms(query).is_empty() {
        return Err(ToolError(format!(
            "`query` has no searchable keyword ({query:?}); use a more distinctive term."
        )));
    }
    let scope = parse_scope(params.scope.as_deref(), MemoryScope::All)?;
    let limit = params
        .limit
        .unwrap_or(DEFAULT_LIST_LIMIT)
        .min(MAX_LIST_LIMIT);
    if limit == 0 {
        return Ok(ToolResult::text("No memories found (limit 0)."));
    }

    let mut scoped = store::read_scoped(scope, cwd).map_err(ToolError)?;
    let ranked = recall::rank(&scoped.active_entries(), query, limit);
    if ranked.is_empty() {
        return Ok(ToolResult::text(format!("No memories matching {query:?}.")));
    }
    touch_hits(&mut scoped, &ranked);

    let mut out = format!("Found {} memories matching {query:?}:\n\n", ranked.len());
    for item in &ranked {
        out.push_str(&render_entry(item.scope, &item.entry));
    }
    Ok(ToolResult::text(out))
}

/// `list`：按更新时间倒序列出条目，可按分类/标签过滤。
fn action_list(params: &Params, cwd: Option<&str>) -> Result<ToolResult, ToolError> {
    let scope = parse_scope(params.scope.as_deref(), MemoryScope::All)?;
    let limit = params
        .limit
        .unwrap_or(DEFAULT_LIST_LIMIT)
        .min(MAX_LIST_LIMIT);
    if limit == 0 {
        return Ok(ToolResult::text("No memories stored (limit 0)."));
    }
    let category = params
        .category
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(MemoryCategory::parse_lenient);
    let tags = params.tags.as_deref();

    let scoped = store::read_scoped(scope, cwd).map_err(ToolError)?;
    let mut entries: Vec<(MemoryScope, MemoryEntry)> = scoped
        .active_entries()
        .into_iter()
        .filter(|(_, entry)| category.is_none_or(|want| entry.category == want))
        .filter(|(_, entry)| {
            tags.is_none_or(|wanted| {
                wanted.iter().all(|tag| {
                    let tag = tag.trim().to_lowercase();
                    entry.tags.iter().any(|have| have.to_lowercase() == tag)
                })
            })
        })
        .map(|(scope, entry)| (scope, entry.clone()))
        .collect();
    entries.sort_by(|a, b| {
        b.1.updated_ms()
            .cmp(&a.1.updated_ms())
            .then_with(|| a.1.id.cmp(&b.1.id))
    });
    entries.truncate(limit);

    if entries.is_empty() {
        return Ok(ToolResult::text("No memories stored."));
    }
    let mut out = format!("Memories ({}):\n\n", entries.len());
    for (scope, entry) in &entries {
        out.push_str(&render_entry(*scope, entry));
    }
    Ok(ToolResult::text(out))
}

/// `forget`：删除一条记忆（先找项目存储，再找全局）。
fn action_forget(params: &Params, cwd: Option<&str>) -> Result<ToolResult, ToolError> {
    let id = require_id(params)?;
    if let Some(mut project) = read_concrete(cwd, MemoryScope::Project)?
        && project.graph.remove_memory(&id).is_some()
    {
        project.save().map_err(|err| ToolError(format!("{err}")))?;
        return Ok(ToolResult::text(format!("Forgot: {id}")));
    }
    if let Some(mut global) = read_concrete(cwd, MemoryScope::Global)?
        && global.graph.remove_memory(&id).is_some()
    {
        global.save().map_err(|err| ToolError(format!("{err}")))?;
        return Ok(ToolResult::text(format!("Forgot: {id}")));
    }
    Ok(ToolResult::text(format!("Not found: {id}")))
}

/// `tag`：给一条记忆加标签。
fn action_tag(params: &Params, cwd: Option<&str>) -> Result<ToolResult, ToolError> {
    let id = require_id(params)?;
    let tags = normalize_tags(params.tags.as_deref());
    if tags.is_empty() {
        return Err(ToolError("`tags` is required for `tag`.".to_string()));
    }

    let mut located = locate_containing(cwd, &id)?;
    let mut added: Vec<&str> = Vec::new();
    let mut removed: Vec<&str> = Vec::new();
    for tag in &tags {
        match tag.strip_prefix('-') {
            Some(name) if !name.is_empty() => {
                located.graph.untag_memory(&id, name);
                removed.push(name);
            }
            _ => {
                located.graph.tag_memory(&id, tag);
                added.push(tag);
            }
        }
    }
    located.save().map_err(|err| ToolError(format!("{err}")))?;

    let mut out = format!("Tagged {id}");
    if !added.is_empty() {
        out.push_str(&format!(" +{}", added.join(", ")));
    }
    if !removed.is_empty() {
        out.push_str(&format!(" -{}", removed.join(", ")));
    }
    Ok(ToolResult::text(out))
}

/// `link`：在两条记忆之间建关联（必须同属一份存储）。
fn action_link(params: &Params, cwd: Option<&str>) -> Result<ToolResult, ToolError> {
    let from_id = params
        .from_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ToolError("`from_id` is required for `link`.".to_string()))?;
    let to_id = params
        .to_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ToolError("`to_id` is required for `link`.".to_string()))?;
    if from_id == to_id {
        return Err(ToolError("`from_id` and `to_id` must differ.".to_string()));
    }
    let weight = params.weight.unwrap_or(0.5).clamp(0.0, 1.0);

    let mut located = locate_containing(cwd, from_id)?;
    if located.graph.get(to_id).is_none() {
        return Err(ToolError(format!(
            "{to_id} is not in the same store as {from_id}; link only works within one scope."
        )));
    }
    located.graph.link_memories(from_id, to_id, weight);
    located.save().map_err(|err| ToolError(format!("{err}")))?;
    Ok(ToolResult::text(format!(
        "Linked {from_id} -> {to_id} (weight {weight:.2})"
    )))
}

/// `related`：从一条记忆出发按图遍历取关联条目。
fn action_related(params: &Params, cwd: Option<&str>) -> Result<ToolResult, ToolError> {
    let id = require_id(params)?;
    let depth = params
        .depth
        .unwrap_or(DEFAULT_RELATED_DEPTH)
        .clamp(1, MAX_RELATED_DEPTH);

    let mut located = locate_containing(cwd, &id)?;
    let scope = scope_of_path(&located.path);
    let related = located.graph.related(&id, depth);
    if related.is_empty() {
        return Ok(ToolResult::text(format!(
            "No related memories found for {id}."
        )));
    }
    let mut out = format!(
        "Found {} memories related to {id} (depth {depth}):\n\n",
        related.len()
    );
    for (entry, score) in &related {
        out.push_str(&render_entry(scope, entry));
        out.push_str(&format!("  relevance: {score:.2}\n"));
    }
    Ok(ToolResult::text(out))
}

/// 必填 `id` 参数。
fn require_id(params: &Params) -> Result<String, ToolError> {
    params
        .id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| ToolError("`id` is required for this action.".to_string()))
}

/// 解析 `scope` 参数（缺省用 `default`）。
fn parse_scope(raw: Option<&str>, default: MemoryScope) -> Result<MemoryScope, ToolError> {
    let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(default);
    };
    raw.parse::<MemoryScope>().map_err(|_| {
        ToolError(format!(
            "invalid `scope` {raw:?}: expected project, global or all"
        ))
    })
}

/// 归一化标签：去空白、小写、去空、去重。
fn normalize_tags(tags: Option<&[String]>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for tag in tags.unwrap_or(&[]) {
        let tag = tag.trim().to_lowercase();
        if tag.is_empty() || out.contains(&tag) {
            continue;
        }
        out.push(tag);
        if out.len() >= 5 {
            break;
        }
    }
    out
}

/// 读一份具体存储；项目作用域无 cwd 时返回 `None`（而不是报错）。
fn read_concrete(cwd: Option<&str>, scope: MemoryScope) -> Result<Option<Located>, ToolError> {
    match scope {
        MemoryScope::Global => store::read_global().map(Some).map_err(ToolError),
        _ => match cwd.filter(|dir| !dir.trim().is_empty()) {
            Some(cwd) => store::read_project(cwd).map(Some).map_err(ToolError),
            None => Ok(None),
        },
    }
}

/// 找到包含 `id` 的那份存储（先项目后全局）。
fn locate_containing(cwd: Option<&str>, id: &str) -> Result<Located, ToolError> {
    if let Some(project) = read_concrete(cwd, MemoryScope::Project)?
        && project.graph.get(id).is_some()
    {
        return Ok(project);
    }
    if let Some(global) = read_concrete(cwd, MemoryScope::Global)?
        && global.graph.get(id).is_some()
    {
        return Ok(global);
    }
    Err(ToolError(format!("memory not found: {id}")))
}

/// 存储路径 → 作用域标签（`.../global.json` 为全局，其余为项目）。
fn scope_of_path(path: &Path) -> MemoryScope {
    if path.file_name().and_then(|n| n.to_str()) == Some("global.json") {
        MemoryScope::Global
    } else {
        MemoryScope::Project
    }
}

/// 渲染一条记忆（两行：标题 + 元信息）。
fn render_entry(scope: MemoryScope, entry: &MemoryEntry) -> String {
    let content = truncate_chars(
        &recall::single_line(&entry.content),
        CONTENT_PREVIEW_CHARS,
        Some(DEF_ELLIPSIS),
        true,
    );
    let mut meta = format!("  id: {}  scope: {}", entry.id, scope.as_label());
    if !entry.tags.is_empty() {
        meta.push_str("  tags: ");
        meta.push_str(&entry.tags.join(", "));
    }
    meta.push_str(&format!(
        "  strength: {}  updated: {}",
        entry.strength,
        short_date(&entry.updated_at)
    ));
    format!("- [{}] {}\n{meta}\n\n", entry.category.as_label(), content)
}

/// 从完整 transcript 里取最后一条真实用户消息作为召回查询。
fn last_user_query(messages: &[AgentMessage]) -> Option<String> {
    let query = messages
        .iter()
        .rev()
        .filter(|message| message.role == "user")
        .map(|message| message.text())
        .map(|text| text.trim().to_string())
        .find(|text| !text.is_empty() && !is_injected_context(text))?;
    Some(truncate_chars(
        &query,
        QUERY_MAX_CHARS,
        Some(DEF_ELLIPSIS),
        true,
    ))
}

/// 是否是宿主/扩展注入的上下文（不当作召回查询）。
fn is_injected_context(text: &str) -> bool {
    let head = text.trim_start();
    head.starts_with("[memory]")
        || head.starts_with("<system-reminder>")
        || head.starts_with("[hang-detect]")
}

/// 计算这一轮的召回块；没有可注入内容时返回 `None`。
fn build_recall_block(cfg: &Config, cwd: Option<&str>, query: &str) -> Option<String> {
    let limit = cfg.recall_limit();
    let budget = cfg.inject_budget();
    if limit == 0 || budget == 0 {
        return None;
    }

    let scope = if cwd.is_some() {
        MemoryScope::All
    } else {
        MemoryScope::Global
    };
    let scoped = store::read_scoped(scope, cwd).ok()?;
    let ranked = recall::rank(&scoped.active_entries(), query, limit);
    recall::format_block(&ranked, budget)
}

/// 记录「这些条目被用到了」：累加访问次数并落盘。
///
/// 只用于模型显式调 `recall` / `search`；自动注入走 [`build_recall_block`]，那条路径在
/// 请求前跑，**不写盘**（否则每次请求都要 fsync 一次记忆图）。
fn touch_hits(scoped: &mut Scoped, ranked: &[Ranked]) {
    let mut changed = false;
    for item in ranked {
        if let Some(graph) = scoped.graph_mut(item.scope)
            && let Some(entry) = graph.get_mut(&item.entry.id)
        {
            entry.touch();
            changed = true;
        }
    }
    if changed {
        _ = scoped.save();
    }
}

/// 推进抽取 cadence：到点就挂起标记，等结算时执行。
fn advance_cadence() {
    let mut st = lock();
    let interval = st.cfg.extract_interval();
    if interval == 0 {
        st.turns_since_extract = 0;
        st.extract_pending = false;
        return;
    }
    st.turns_since_extract = st.turns_since_extract.saturating_add(1);
    if st.turns_since_extract >= interval {
        st.turns_since_extract = 0;
        st.extract_pending = true;
    }
}

/// 结算时发起抽取（异步、失败只记录不打扰用户）。
fn maybe_spawn_extraction(event: &Value) {
    let (cfg, cwd, session_id, model) = {
        let mut st = lock();
        if !st.extract_pending || st.extracting {
            st.extract_pending = false;
            return;
        }
        st.extract_pending = false;
        (
            st.cfg.clone(),
            st.cwd.clone(),
            st.session_id.clone(),
            st.model.clone(),
        )
    };

    if cfg.extract_interval() == 0 || runtime::offline() {
        return;
    }
    let Some((provider, model_id)) = model else {
        return;
    };
    let Some(excerpt) = excerpt_from_event(event) else {
        return;
    };
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };

    lock().extracting = true;
    let source = session_id.clone().unwrap_or_else(|| "extract".to_string());
    handle.spawn(async move {
        let known = known_contents(cwd.as_deref());
        let note = match run_extraction(&provider, &model_id, session_id, &excerpt, &known).await {
            Ok(items) => apply_extracted(&items, cwd.as_deref(), &source),
            Err(err) => ExtractNote {
                at: now_iso(),
                added: 0,
                reinforced: 0,
                error: Some(err),
            },
        };
        let mut st = lock();
        st.extracting = false;
        if note.error.is_none() && (note.added > 0 || note.reinforced > 0) {
            st.recall_cache = None; // 新条目下一次请求就该能被召回
        }
        st.last_extract = Some(note);
    });
}

/// 从边界事件里取对话摘录。
fn excerpt_from_event(event: &Value) -> Option<String> {
    let values = event.get("contextMessages")?.as_array()?;
    let messages: Vec<AgentMessage> = values
        .iter()
        .filter_map(|value| serde_json::from_value(value.clone()).ok())
        .collect();
    let excerpt =
        extract::build_excerpt(&messages, extract::EXCERPT_MESSAGES, extract::EXCERPT_CHARS);
    (!excerpt.trim().is_empty()).then_some(excerpt)
}

/// 抽取提示里的「已知条目」：有效条目的正文，单条截断。
fn known_contents(cwd: Option<&str>) -> Vec<String> {
    let scope = if cwd.is_some() {
        MemoryScope::All
    } else {
        MemoryScope::Global
    };
    let Ok(scoped) = store::read_scoped(scope, cwd) else {
        return Vec::new();
    };
    scoped
        .active_entries()
        .into_iter()
        .take(extract::MAX_EXTRACTED * 8)
        .map(|(_, entry)| {
            truncate_chars(
                &recall::single_line(&entry.content),
                KNOWN_PREVIEW_CHARS,
                Some(DEF_ELLIPSIS),
                true,
            )
        })
        .collect()
}

/// 一次性抽取请求：刷新 OAuth → 解析模型 → 非流式补全 → 解析 JSON。
async fn run_extraction(
    provider: &str,
    model_id: &str,
    session_id: Option<String>,
    excerpt: &str,
    known: &[String],
) -> Result<Vec<ExtractedMemory>, String> {
    auth::ensure_oauth_valid(provider)
        .await
        .map_err(|err| format!("{err}"))?;
    let model = model_resolver::configured_model(provider, model_id, None, session_id)
        .map_err(|err| format!("{err}"))?;
    let max_tokens = extract::output_cap(&model, extract::MAX_TOKENS);
    let prompt = extract::build_prompt(excerpt, known);
    let (text, _usage) =
        provider::simple_completion(&model, extract::SYSTEM, &prompt, Some(max_tokens))
            .await
            .map_err(|err| format!("{err}"))?;
    Ok(extract::parse_response(&text))
}

/// 把抽取结果写进两份存储，返回结果摘要。
fn apply_extracted(
    items: &[extract::ExtractedMemory],
    cwd: Option<&str>,
    source: &str,
) -> ExtractNote {
    let mut added = 0;
    let mut reinforced = 0;
    let mut errors: Vec<String> = Vec::new();

    for (scope, group) in group_by_scope(items) {
        let located = match scope {
            MemoryScope::Global => store::read_global(),
            _ => match cwd.filter(|dir| !dir.trim().is_empty()) {
                Some(cwd) => store::read_project(cwd),
                None => {
                    errors.push("no working directory for project-scoped memory".to_string());
                    continue;
                }
            },
        };
        let mut located = match located {
            Ok(located) => located,
            Err(err) => {
                errors.push(err);
                continue;
            }
        };
        let (a, r) = merge_into(&mut located.graph, &group, source);
        added += a;
        reinforced += r;
        if let Err(err) = located.save() {
            errors.push(format!("{err}"));
        }
    }

    ExtractNote {
        at: now_iso(),
        added,
        reinforced,
        error: (!errors.is_empty()).then(|| errors.join("; ")),
    }
}

/// 按作用域分组抽取结果（保持各自顺序）。
fn group_by_scope(items: &[ExtractedMemory]) -> Vec<(MemoryScope, Vec<ExtractedMemory>)> {
    let mut groups: Vec<(MemoryScope, Vec<ExtractedMemory>)> = Vec::new();
    for item in items {
        match groups.iter_mut().find(|(scope, _)| *scope == item.scope) {
            Some((_, group)) => group.push(item.clone()),
            None => groups.push((item.scope, vec![item.clone()])),
        }
    }
    groups
}

/// 把一批条目并进一份图：同分类同正文视为强化，否则新增。返回 `(新增, 强化)`。
fn merge_into(graph: &mut MemoryGraph, items: &[ExtractedMemory], source: &str) -> (usize, usize) {
    let mut added = 0;
    let mut reinforced = 0;
    for item in items {
        let duplicate = graph
            .active_memories()
            .find(|entry| {
                entry.category == item.category
                    && entry
                        .content
                        .trim()
                        .eq_ignore_ascii_case(item.content.trim())
            })
            .map(|entry| entry.id.clone());
        if let Some(id) = duplicate {
            if let Some(entry) = graph.get_mut(&id) {
                entry.reinforce(source);
            }
            reinforced += 1;
            continue;
        }
        graph.add_memory(
            MemoryEntry::new(item.category, item.content.clone())
                .with_source(source)
                .with_tags(item.tags.clone())
                .with_trust(item.trust),
        );
        added += 1;
    }
    (added, reinforced)
}

/// `/memory` handler：打印状态、列条目、检索、删除，或打开设置面板。
fn command_memory(st: &mut App, raw: &str) -> bool {
    let arg = command_arg(raw);
    let mut parts = arg.split_whitespace();
    let sub = parts.next().unwrap_or("status");
    let rest: Vec<&str> = parts.collect();
    let cwd = Some(st.cwd.as_str()).filter(|dir| !dir.trim().is_empty());

    let text = match sub {
        "status" | "" => memory_status(cwd),
        "list" => memory_list(cwd, rest.first().copied()),
        "search" => match rest.first() {
            Some(_) => memory_search(cwd, &rest.join(" ")),
            None => "Usage: /memory search <query>".to_string(),
        },
        "forget" => match rest.first() {
            Some(id) => memory_forget(cwd, id),
            None => "Usage: /memory forget <id>".to_string(),
        },
        "settings" => {
            extensions::request_ui(ExtensionUiRequest::ShowSettings {
                ext: EXT.to_string(),
            });
            return false;
        }
        other => {
            format!("unknown subcommand {other:?}: use status, list, search, forget or settings")
        }
    };

    st.push_msg(text, MsgLevel::Info);
    false
}

/// `/memory status` 的文本。
fn memory_status(cwd: Option<&str>) -> String {
    let (cfg, last, extracting, model) = {
        let st = lock();
        (
            st.cfg.clone(),
            st.last_extract.clone(),
            st.extracting,
            st.model.clone(),
        )
    };

    let (project_count, tag_count, edge_count) = match cwd {
        Some(cwd) => match store::read_project(cwd) {
            Ok(located) => (
                located.graph.active_memories().count(),
                located.graph.sorted_tags().len(),
                located.graph.edge_count(),
            ),
            Err(err) => return format!("memory store unreadable: {err}"),
        },
        None => (0, 0, 0),
    };
    let global_count = match store::read_global() {
        Ok(located) => located.graph.active_memories().count(),
        Err(err) => return format!("memory store unreadable: {err}"),
    };

    let mut out = format!(
        "memory · {project_count} project / {global_count} global\n\
         tags: {tag_count} · edges: {edge_count}\n\
         auto recall: {} · extract every {} · budget {} chars\n\
         storage: {}",
        cfg.recall_limit(),
        cfg.extract_interval(),
        cfg.inject_budget(),
        store::memory_dir().display()
    );
    match (extracting, &model) {
        (true, Some((provider, model_id))) => {
            out.push_str(&format!("\nextracting with {provider}/{model_id}…"));
        }
        (true, None) => out.push_str("\nextracting…"),
        (false, _) => {}
    }
    match last {
        Some(note) => {
            out.push_str(&format!(
                "\nlast extract {}: +{}/~{}",
                note.at, note.added, note.reinforced
            ));
            if let Some(err) = note.error {
                out.push_str(&format!("\n  failed: {err}"));
            }
        }
        None => out.push_str("\nlast extract: never"),
    }
    out
}

/// `/memory list [scope]` 的文本。
fn memory_list(cwd: Option<&str>, scope_arg: Option<&str>) -> String {
    let scope = match parse_scope(scope_arg, MemoryScope::All) {
        Ok(scope) => scope,
        Err(err) => return err.0,
    };
    let scoped = match store::read_scoped(scope, cwd) {
        Ok(scoped) => scoped,
        Err(err) => return err,
    };
    let mut entries: Vec<(MemoryScope, MemoryEntry)> = scoped
        .active_entries()
        .into_iter()
        .map(|(scope, entry)| (scope, entry.clone()))
        .collect();
    if entries.is_empty() {
        return "No memories stored.".to_string();
    }
    entries.sort_by(|a, b| b.1.updated_ms().cmp(&a.1.updated_ms()));
    entries.truncate(MAX_LIST_LIMIT);

    let mut out = format!("Memories ({}):\n\n", entries.len());
    for (scope, entry) in &entries {
        out.push_str(&render_entry(*scope, entry));
    }
    out.trim_end().to_string()
}

/// `/memory search <query>` 的文本。
fn memory_search(cwd: Option<&str>, query: &str) -> String {
    let query = query.trim();
    if recall::query_terms(query).is_empty() {
        return format!("no searchable keyword in {query:?}");
    }
    let scope = if cwd.is_some() {
        MemoryScope::All
    } else {
        MemoryScope::Global
    };
    let scoped = match store::read_scoped(scope, cwd) {
        Ok(scoped) => scoped,
        Err(err) => return err,
    };
    let ranked = recall::rank(&scoped.active_entries(), query, MAX_LIST_LIMIT);
    if ranked.is_empty() {
        return format!("No memories matching {query:?}.");
    }
    let mut out = format!("Found {} memories matching {query:?}:\n\n", ranked.len());
    for item in &ranked {
        out.push_str(&render_entry(item.scope, &item.entry));
    }
    out.trim_end().to_string()
}

/// `/memory forget <id>` 的文本。
fn memory_forget(cwd: Option<&str>, id: &str) -> String {
    let params = Params {
        id: Some(id.to_string()),
        ..Params::default()
    };
    match action_forget(&params, cwd) {
        Ok(result) => result.text,
        Err(err) => err.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::AgentDirGuard;
    use extract::ExtractedMemory;
    use graph::TrustLevel;
    use std::sync::{Mutex as StdMutex, MutexGuard as StdMutexGuard, OnceLock as StdOnceLock};

    /// 串行化「改全局 state 配置」的测试。
    ///
    /// **不能**用 `lock()` 自己来串行：`std::sync::Mutex` 不可重入，持着它再调 `lock()`
    /// 会自死锁。所以这里另起一把叶级测试锁，锁序恒为「测试锁 → state mutex」。
    static CADENCE_LOCK: StdOnceLock<StdMutex<()>> = StdOnceLock::new();

    /// 取 cadence 测试的串行锁。
    fn cadence_guard() -> StdMutexGuard<'static, ()> {
        CADENCE_LOCK
            .get_or_init(|| StdMutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// 把 state 里的配置换成给定 JSON 并复位 cadence 字段。
    fn set_state_config(interval: i64) {
        let mut st = lock();
        st.cfg = merge_config(Some(&json!({
            "recallLimit": 5,
            "extractInterval": interval,
            "injectBudgetChars": 8000,
            "configVersion": 1
        })));
        st.turns_since_extract = 0;
        st.extract_pending = false;
    }

    /// 把 state 配置恢复成缺省，避免污染同一进程内的其它用例。
    fn restore_state_config() {
        let mut st = lock();
        st.cfg = merge_config(None);
        st.turns_since_extract = 0;
        st.extract_pending = false;
    }

    /// 限定一个项目目录下的记忆操作。
    struct Fixture {
        _agent: AgentDirGuard,
        _project: tempfile::TempDir,
        cwd: String,
    }

    impl Fixture {
        fn new() -> Self {
            let agent = AgentDirGuard::temp();
            let project = tempfile::tempdir().unwrap();
            let cwd = project.path().to_string_lossy().to_string();
            Self {
                _agent: agent,
                _project: project,
                cwd,
            }
        }
    }

    fn call(fixture: &Fixture, args: Value) -> Result<String, ToolError> {
        run_action(&args, Some(&fixture.cwd), Some("session-test")).map(|result| result.text)
    }

    #[test]
    fn remember_then_list_and_recall_round_trip() {
        let fixture = Fixture::new();
        let out = call(
            &fixture,
            json!({"action":"remember","content":"仓库用 make check 而不是 cargo test","category":"fact","tags":["工具链"]}),
        )
        .unwrap();
        assert!(out.contains("Remembered (fact / project)"));
        let id = out
            .rsplit_once("id: ")
            .map(|(_, tail)| tail.trim().to_string())
            .unwrap();

        let list = call(&fixture, json!({"action":"list"})).unwrap();
        assert!(list.contains("make check"), "{list}");
        assert!(list.contains(&id), "{list}");

        let recalled = call(&fixture, json!({"action":"recall","query":"make check"})).unwrap();
        assert!(recalled.contains("make check"), "{recalled}");

        // 中文查询按二字组合召回：查询词的 bigram 命中正文/标签即可
        let recalled_cn = call(&fixture, json!({"action":"recall","query":"仓库工具链"})).unwrap();
        assert!(
            recalled_cn.contains("make check"),
            "中文查询要能召回: {recalled_cn}"
        );
        let searched_cn = call(&fixture, json!({"action":"search","query":"工具链"})).unwrap();
        assert!(
            searched_cn.contains("make check"),
            "中文检索要能命中: {searched_cn}"
        );
    }

    #[test]
    fn remember_deduplicates_within_same_category_and_scope() {
        let fixture = Fixture::new();
        call(
            &fixture,
            json!({"action":"remember","content":"prefers pnpm"}),
        )
        .unwrap();
        let second = call(
            &fixture,
            json!({"action":"remember","content":"prefers pnpm"}),
        )
        .unwrap();
        assert!(second.contains("Already known"), "{second}");
        assert!(second.contains("strength: 2"), "{second}");
        let list = call(&fixture, json!({"action":"list"})).unwrap();
        assert_eq!(list.matches("id: ").count(), 1, "同一条不会重复写入");
    }

    #[test]
    fn project_and_global_stores_are_isolated() {
        let fixture = Fixture::new();
        call(
            &fixture,
            json!({"action":"remember","content":"project fact"}),
        )
        .unwrap();
        call(
            &fixture,
            json!({"action":"remember","content":"global preference","scope":"global"}),
        )
        .unwrap();

        let project = call(&fixture, json!({"action":"list","scope":"project"})).unwrap();
        assert!(project.contains("project fact") && !project.contains("global preference"));
        let global = call(&fixture, json!({"action":"list","scope":"global"})).unwrap();
        assert!(global.contains("global preference") && !global.contains("project fact"));
        let all = call(&fixture, json!({"action":"list","scope":"all"})).unwrap();
        assert!(all.contains("project fact") && all.contains("global preference"));
    }

    #[test]
    fn remember_rejects_scope_all_and_empty_content() {
        let fixture = Fixture::new();
        let err = call(
            &fixture,
            json!({"action":"remember","content":"x","scope":"all"}),
        )
        .unwrap_err();
        assert!(err.0.contains("concrete scope"), "{err:?}");
        let err = call(&fixture, json!({"action":"remember","content":"   "})).unwrap_err();
        assert!(err.0.contains("content"), "{err:?}");
    }

    #[test]
    fn forget_removes_across_scopes() {
        let fixture = Fixture::new();
        let out = call(
            &fixture,
            json!({"action":"remember","content":"global thing","scope":"global"}),
        )
        .unwrap();
        let id = out.rsplit_once("id: ").unwrap().1.trim().to_string();

        let forgotten = call(&fixture, json!({"action":"forget","id":id})).unwrap();
        assert!(forgotten.starts_with("Forgot:"));
        let global = call(&fixture, json!({"action":"list","scope":"global"})).unwrap();
        assert_eq!(global, "No memories stored.");
        assert!(
            call(&fixture, json!({"action":"forget","id":id}))
                .unwrap()
                .starts_with("Not found:")
        );
    }

    #[test]
    fn tag_link_and_related_work_together() {
        let fixture = Fixture::new();
        let a_out = call(&fixture, json!({"action":"remember","content":"uses pnpm"})).unwrap();
        let a = a_out.rsplit_once("id: ").unwrap().1.trim().to_string();
        let b_out = call(
            &fixture,
            json!({"action":"remember","content":"lockfile is pnpm-lock"}),
        )
        .unwrap();
        let b = b_out.rsplit_once("id: ").unwrap().1.trim().to_string();

        let tagged = call(&fixture, json!({"action":"tag","id":a,"tags":["Tooling"]})).unwrap();
        assert!(
            tagged.contains("tagged") || tagged.contains("Tagged"),
            "{tagged}"
        );
        let list = call(&fixture, json!({"action":"list"})).unwrap();
        assert!(list.contains("tags: tooling"), "标签被小写化: {list}");

        let linked = call(
            &fixture,
            json!({"action":"link","from_id":a,"to_id":b,"weight":0.9}),
        )
        .unwrap();
        assert!(linked.contains("Linked"), "{linked}");

        let related = call(&fixture, json!({"action":"related","id":a,"depth":1})).unwrap();
        assert!(related.contains("pnpm-lock"), "{related}");
        assert_eq!(
            related.matches("id: ").count(),
            1,
            "related 只列出关联条目: {related}"
        );
    }

    #[test]
    fn link_requires_both_ends_in_the_same_store() {
        let fixture = Fixture::new();
        let a = call(&fixture, json!({"action":"remember","content":"a"})).unwrap();
        let a = a.rsplit_once("id: ").unwrap().1.trim().to_string();
        let b = call(
            &fixture,
            json!({"action":"remember","content":"b","scope":"global"}),
        )
        .unwrap();
        let b = b.rsplit_once("id: ").unwrap().1.trim().to_string();

        let err = call(&fixture, json!({"action":"link","from_id":a,"to_id":b})).unwrap_err();
        assert!(err.0.contains("same store"), "{err:?}");
    }

    #[test]
    fn unknown_action_and_missing_id_are_reported() {
        let fixture = Fixture::new();
        let err = call(&fixture, json!({"action":"nope"})).unwrap_err();
        assert!(err.0.contains("unknown `memory` action"), "{err:?}");
        let err = call(&fixture, json!({"action":"forget"})).unwrap_err();
        assert!(err.0.contains("`id` is required"), "{err:?}");
        let err = call(&fixture, json!({"action":"search","query":"the and of"})).unwrap_err();
        assert!(err.0.contains("no searchable keyword"), "{err:?}");
    }

    #[test]
    fn recall_without_query_returns_recent_entries() {
        let fixture = Fixture::new();
        call(
            &fixture,
            json!({"action":"remember","content":"one","category":"fact"}),
        )
        .unwrap();
        call(
            &fixture,
            json!({"action":"remember","content":"two","category":"correction"}),
        )
        .unwrap();
        let out = call(&fixture, json!({"action":"recall","limit":1})).unwrap();
        assert!(out.contains("Most relevant 1 memories"), "{out}");
        assert!(out.contains("two"), "纠正类优先: {out}");
    }

    #[test]
    fn recall_limit_zero_returns_nothing() {
        let fixture = Fixture::new();
        call(&fixture, json!({"action":"remember","content":"x"})).unwrap();
        let out = call(&fixture, json!({"action":"recall","limit":0})).unwrap();
        assert!(!out.contains("x"), "{out}");
        let out = call(&fixture, json!({"action":"list","limit":0})).unwrap();
        assert!(!out.contains("x"), "{out}");
    }

    #[test]
    fn recall_block_is_stable_and_marks_itself() {
        let fixture = Fixture::new();
        call(
            &fixture,
            json!({"action":"remember","content":"uses pnpm for installs"}),
        )
        .unwrap();
        let cfg = merge_config(None);

        let first = build_recall_block(&cfg, Some(&fixture.cwd), "pnpm install").unwrap();
        let second = build_recall_block(&cfg, Some(&fixture.cwd), "pnpm install").unwrap();
        assert_eq!(first, second, "同图同查询必得同文本");
        assert!(first.starts_with("[memory]"), "{first}");
        assert!(first.contains("uses pnpm for installs"));

        assert!(
            build_recall_block(&cfg, Some(&fixture.cwd), "unrelated zebra").is_none(),
            "无命中不产生注入块"
        );
    }

    #[test]
    fn recall_block_disabled_by_zero_settings() {
        let fixture = Fixture::new();
        call(&fixture, json!({"action":"remember","content":"uses pnpm"})).unwrap();
        let cfg = merge_config(Some(&json!({
            "recallLimit": 0,
            "extractInterval": 0,
            "injectBudgetChars": 8000,
            "configVersion": 1
        })));
        assert!(build_recall_block(&cfg, Some(&fixture.cwd), "pnpm").is_none());

        let cfg = merge_config(Some(&json!({
            "recallLimit": 5,
            "extractInterval": 0,
            "injectBudgetChars": 0,
            "configVersion": 1
        })));
        assert!(build_recall_block(&cfg, Some(&fixture.cwd), "pnpm").is_none());
    }

    #[test]
    fn last_user_query_skips_injected_context() {
        let mut injected = AgentMessage::user_text("[memory] old block");
        injected.role = "user".to_string();
        let messages = vec![
            AgentMessage::user_text("<system-reminder>tasks</system-reminder>"),
            AgentMessage::user_text("real question"),
            injected,
        ];
        assert_eq!(last_user_query(&messages).as_deref(), Some("real question"));
        assert!(last_user_query(&[]).is_none());
        assert!(last_user_query(&[AgentMessage::user_text("   ")]).is_none());
    }

    #[test]
    fn cadence_advances_and_arms_extraction() {
        let _serial = cadence_guard();
        set_state_config(2);

        advance_cadence();
        assert!(!lock().extract_pending, "第一次还不够");
        advance_cadence();
        assert!(lock().extract_pending, "到间隔应挂起");
        assert_eq!(lock().turns_since_extract, 0, "计数复位");

        restore_state_config();
    }

    #[test]
    fn cadence_is_off_when_interval_is_zero() {
        let _serial = cadence_guard();
        set_state_config(0);

        advance_cadence();
        advance_cadence();
        assert!(!lock().extract_pending);
        assert_eq!(lock().turns_since_extract, 0);

        restore_state_config();
    }

    #[test]
    fn merge_into_reinforces_duplicates_and_adds_new_items() {
        let mut graph = MemoryGraph::new();
        let item = |content: &str, category| ExtractedMemory {
            category,
            content: content.to_string(),
            tags: vec!["t".to_string()],
            scope: MemoryScope::Project,
            trust: TrustLevel::Medium,
        };
        let first = vec![item("uses pnpm", MemoryCategory::Preference)];
        assert_eq!(merge_into(&mut graph, &first, "extract"), (1, 0));

        let again = vec![
            item("uses pnpm", MemoryCategory::Preference),
            item("uses pnpm", MemoryCategory::Fact),
            item("repo uses justfile", MemoryCategory::Fact),
        ];
        let (added, reinforced) = merge_into(&mut graph, &again, "extract");
        assert_eq!((added, reinforced), (2, 1), "同分类同正文强化，其它新增");
        assert_eq!(graph.active_memories().count(), 3);
    }

    #[test]
    fn apply_extracted_writes_to_the_right_scope() {
        let fixture = Fixture::new();
        let items = vec![
            ExtractedMemory {
                category: MemoryCategory::Fact,
                content: "project fact".to_string(),
                tags: Vec::new(),
                scope: MemoryScope::Project,
                trust: TrustLevel::Medium,
            },
            ExtractedMemory {
                category: MemoryCategory::Preference,
                content: "global preference".to_string(),
                tags: Vec::new(),
                scope: MemoryScope::Global,
                trust: TrustLevel::High,
            },
        ];
        let note = apply_extracted(&items, Some(&fixture.cwd), "extract");
        assert_eq!((note.added, note.reinforced), (2, 0));
        assert!(note.error.is_none());

        let project = call(&fixture, json!({"action":"list","scope":"project"})).unwrap();
        assert!(project.contains("project fact") && !project.contains("global preference"));
        let global = call(&fixture, json!({"action":"list","scope":"global"})).unwrap();
        assert!(global.contains("global preference") && !global.contains("project fact"));
    }

    #[test]
    fn apply_extracted_reports_missing_cwd_for_project_items() {
        let _agent = AgentDirGuard::temp();
        let items = vec![ExtractedMemory {
            category: MemoryCategory::Fact,
            content: "needs a project".to_string(),
            tags: Vec::new(),
            scope: MemoryScope::Project,
            trust: TrustLevel::Medium,
        }];
        let note = apply_extracted(&items, None, "extract");
        assert_eq!(note.added, 0);
        assert!(
            note.error
                .as_deref()
                .unwrap_or("")
                .contains("working directory")
        );
    }

    #[test]
    fn scope_of_path_distinguishes_global() {
        assert_eq!(
            scope_of_path(std::path::Path::new("/x/memory/global.json")),
            MemoryScope::Global
        );
        assert_eq!(
            scope_of_path(std::path::Path::new("/x/memory/projects/--a--.json")),
            MemoryScope::Project
        );
    }

    #[test]
    fn command_helpers_render_status_and_list() {
        let fixture = Fixture::new();
        call(
            &fixture,
            json!({"action":"remember","content":"note for status"}),
        )
        .unwrap();

        let status = memory_status(Some(&fixture.cwd));
        assert!(status.contains("memory · 1 project / 0 global"), "{status}");
        assert!(status.contains("storage:"), "{status}");
        assert!(
            status.contains("last extract: never") || status.contains("last extract"),
            "{status}"
        );

        let list = memory_list(Some(&fixture.cwd), None);
        assert!(list.contains("note for status"), "{list}");
        let search = memory_search(Some(&fixture.cwd), "status");
        assert!(search.contains("note for status"), "{search}");
        let none = memory_search(Some(&fixture.cwd), "the and of");
        assert!(none.contains("no searchable keyword"), "{none}");
    }

    #[test]
    fn command_surface_is_declared() {
        let ext = Memory;
        assert_eq!(ext.name(), EXT);
        assert!(!ext.default_enabled(), "默认不启用");
        assert_eq!(
            ext.modes(),
            vec![ExtensionMode::Dev, ExtensionMode::Creator]
        );
        assert_eq!(ext.tools().len(), 1);
        assert_eq!(ext.tools()[0].name, TOOL_MEMORY);
        assert_eq!(
            ext.tools()[0].exposure,
            ToolExposure::Direct,
            "memory 工具必须直接暴露：延迟后 snippet/guidelines 不进系统提示，remember 失去驱动"
        );
        assert!(
            ext.prerequisite_tools().is_empty(),
            "直接工具不依赖 tool_search，前置声明会误报 requires"
        );
        let cmds = ext.commands();
        assert_eq!(cmds.len(), 1);
        assert!(cmds[0].busy_safe);
        assert!(ext.hooks().contains(&ExtensionHook::ContextWithSystem));
        assert!(ext.hooks().contains(&ExtensionHook::Boundary));
        assert!(ext.fork_project().is_some());
    }
}
