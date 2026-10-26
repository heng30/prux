//! `session-search` 扩展：在**历史会话**里做关键词检索。
//!
//! 提供 `session_search` 工具，扫描 `agent_dir()/sessions/<编码目录>/*.jsonl`
//! （按修改时间取最新一批，默认 [`DEFAULT_MAX_SCAN_SESSIONS`] 份），把命中的片段
//! **按会话分组**交回模型——解决「会话一关，之前定过的方案、跑过的命令就再也想不起来」。
//!
//! 面向「回忆」而不是「原始 grep」，因此默认值都往这个方向调：
//! - **隐藏当前会话**（`include_current=false`）：否则几乎必然搜到它自己；
//! - **隐藏工具调用与结果**（`include_tools=false`）：工具日志会把结论淹没；
//! - **隐藏 `<system-reminder>` 开头的注入消息**；
//! - **每个会话只出 1 条**（`max_per_session`）：结果按会话铺开而不是被一份会话刷屏；
//! - 每条命中只给一段**围绕命中点的摘录**（[`EXCERPT_CHARS`]），不整条消息灌进上下文。
//!
//! 可检索的内容：user / assistant 消息（含 thinking 块）、压缩摘要与分支摘要、
//! 以及会话名（名字是用户亲手写的，命中时作为一条独立的 `session` 结果）。
//!
//! 检索是**词法**的（无 embedding，见迁移档 §4.15）：词元切分与命中摘录由
//! [`crate::utils::text`] 提供，本模块只管「扫会话 + 打分 + 分组」。查询按非字母数字切成
//! 词元、小写化、去停用词；中日韩文字不靠空格断词，连续 CJK 串先按相邻二字组合（bigram）
//! 拆开——否则「如何修复缓存失效」整句会变成一个词元，只能逐字连续命中原文才搜得到。
//! 纯停用词查询直接报错。
//!
//! 边界：**只搜自家会话**。别家 harness 的私有 jsonl 不读（迁移档 §4.10）。
//!
//! 默认**不启用**（`Dev` / `Creator`）：它是可选的回忆工具，不该默认占用工具位。

use crate::{
    APP_NAME,
    core::{
        extensions::{
            Extension, ExtensionMode, ExtensionTool, ForkProjectInfo, ToolAnnotations, ToolExecCtx,
            ToolExposure,
        },
        provider::{AgentMessage, ContentBlock},
        session_manager::{self, OpenSessionResult, Session},
        session_v4::Entry,
        settings_manager,
        tools::{ToolError, ToolResult},
    },
    extensions::{EXTENSION_FACTORIES, ExtensionFactory, PRIORITY_SESSION_SEARCH, tool_search},
    utils::{
        display::flatten_line,
        text::{self, excerpt_containing},
        time::{format_local, parse_date_bound_ms, system_to_ms},
        truncate::truncate_chars,
    },
};
use futures_util::future::BoxFuture;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, OnceLock},
    time::UNIX_EPOCH,
};
use strum_macros::{EnumString, IntoStaticStr};

/// 扩展名（也是 `/extension` 面板中的标识）。
const EXT: &str = "session-search";

/// 工具名（模型可见）。
const TOOL_SESSION_SEARCH: &str = "session_search";

/// 一次检索返回的命中条数上限（`limit` 的默认值与上界）。
const DEFAULT_LIMIT: usize = 10;
/// `limit` 参数的取值上界。
const MAX_LIMIT: usize = 50;

/// 同一会话最多贡献的命中条数（`max_per_session` 的默认值与上界）。
const DEFAULT_MAX_PER_SESSION: usize = 1;
/// `max_per_session` 参数的取值上界。
const MAX_MAX_PER_SESSION: usize = 20;

/// 默认扫描的会话文件份数（按修改时间取最新一批）。
const DEFAULT_MAX_SCAN_SESSIONS: usize = 200;
/// `max_scan_sessions` 参数的取值上界。
const MAX_MAX_SCAN_SESSIONS: usize = 2000;

/// 每条命中前后附带的消息条数上限（`context` 的默认值与上界）。
const DEFAULT_CONTEXT: usize = 0;
/// `context` 参数的取值上界。
const MAX_CONTEXT: usize = 5;

/// 查询最多取用的词元个数，防止超长查询把打分拖慢。
///
/// 中文长句按二字组合展开后可能触及这个上限，超出部分不参与检索（前段仍能命中）。
const MAX_TERMS: usize = 16;

/// 单个词元在一条文本里计数的上限：一个词重复 100 次不该压过其它词元的命中。
const MAX_TERM_HITS: usize = 8;

/// 命中多个不同词元时，每多一个词元追加的加权分。
const DISTINCT_TERM_BONUS: f64 = 5.0;

/// 会话名命中的得分倍率：名字是用户亲手写的，信噪比高于正文。
const NAME_SCORE_MULTIPLIER: f64 = 2.0;

/// 单条命中的摘录长度（字符，含首尾省略号）。
const EXCERPT_CHARS: usize = 220;

/// 命中前后相邻消息的展示长度（字符）。
const CONTEXT_CHARS: usize = 120;

/// 未命名会话的标题长度（取首条用户消息的开头，字符）。
const TITLE_CHARS: usize = 60;

/// 单条消息参与检索的文本上限（字符）：超长消息只取前段。
const MESSAGE_TEXT_CAP: usize = 50_000;

/// 单份会话参与检索的文本总量上限（字符）：超长会话只扫前段，避免一次检索读穿内存。
const SESSION_TEXT_BUDGET: usize = 4_000_000;

/// 展示用的角色标签：会话名命中。
const LABEL_SESSION: &str = "session";

/// 自声明工厂：linkme 分布式切片。
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static SESSION_SEARCH_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_SESSION_SEARCH,
    make: || -> Arc<dyn Extension> { Arc::new(SessionSearch) },
};

/// 扩展进程级状态（只有一项：当前会话文件路径）。
#[derive(Default)]
struct State {
    /// 当前会话文件路径，由 `on_session_start` / `on_session_switched` 更新；
    /// `None` 表示不知道（此时用「当前目录最新一份」近似，见 [`current_session_file`]）。
    session_file: Option<PathBuf>,
}

/// 取扩展状态的进程级单例（惰性初始化）。
fn state() -> &'static Mutex<State> {
    /// 扩展状态的进程级单例。
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(State::default()))
}

/// 取状态锁；锁被毒化时取回内部值。
fn lock() -> MutexGuard<'static, State> {
    state().lock().unwrap_or_else(|e| e.into_inner())
}

/// `session-search` 扩展。
pub struct SessionSearch;

impl Extension for SessionSearch {
    /// 扩展名 `session-search`。
    fn name(&self) -> &str {
        EXT
    }

    /// 返回 `/extension` 面板展示的一句话描述。
    fn description(&self) -> &str {
        "Search past chat sessions for relevant content (session_search)."
    }

    /// 声明本扩展移植自 jcode 0.94.0，供 `/extension` 面板展示来源链接。
    fn fork_project(&self) -> Option<ForkProjectInfo> {
        Some(ForkProjectInfo {
            plugin_name: "jcode".to_string(),
            plugin_version: "0.94.0".to_string(),
            url: "https://github.com/1jehuang/jcode".to_string(),
        })
    }

    /// 前置工具：`session_search` 是延迟工具，模型得先用 `tool_search` 才能发现它。
    fn prerequisite_tools(&self) -> Vec<String> {
        vec![tool_search::TOOL_NAME.to_string()]
    }

    /// 默认关闭：回忆工具按需开启，不该默认占用工具位。
    fn default_enabled(&self) -> bool {
        false
    }

    /// 只在 `Dev` / `Creator` 扩展模式下可用。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Dev, ExtensionMode::Creator]
    }

    /// 提供 `session_search` 工具。
    fn tools(&self) -> Vec<ExtensionTool> {
        vec![session_search_tool()]
    }

    /// 启动时记住当前会话文件（默认要从结果里排掉它）。
    fn on_session_start(&self, _cwd: &str, _messages: &[AgentMessage], session: Option<&Session>) {
        remember_session(session.and_then(|s| s.session_file.clone()));
    }

    /// 会话切换后改记新会话文件。
    fn on_session_switched(&self, session_path: Option<&str>, _messages: &[AgentMessage]) {
        remember_session(session_path.map(PathBuf::from));
    }

    /// 启用时清掉记录：禁用期间发生的会话切换不会回调本扩展，留着旧路径会排掉**错误的**
    /// 会话，不如退回 [`current_session_file`] 的近似判定。
    fn on_enabled_changed(&self, enabled: bool) {
        if enabled {
            remember_session(None);
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
            match name.as_str() {
                TOOL_SESSION_SEARCH => run_session_search(&args, &ctx).await,
                other => Err(ToolError(format!("extension {EXT}: unknown tool: {other}"))),
            }
        })
    }
}

/// 记住当前会话文件路径（`None` 时清空，退回近似判定）。
fn remember_session(path: Option<PathBuf>) {
    lock().session_file = path;
}

/// 构造 `session_search` 工具定义（描述、参数 JSON Schema 与 snippet）。
fn session_search_tool() -> ExtensionTool {
    ExtensionTool {
        default_active: true,
        exposure: ToolExposure::Deferred,
        namespace: None,
        annotations: ToolAnnotations {
            read_only: true,
            destructive: false,
            idempotent: true,
        },
        output_schema: None,
        name: TOOL_SESSION_SEARCH.to_string(),
        description: "Search saved past chat sessions on this machine. Results are excerpts grouped by session; the active session and tool traffic are hidden by default. Use it to recall what was decided, tried or run in earlier sessions.".to_string(),
        label: Some("Session Search".to_string()),
        parameters: json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Keywords to look for. Use distinctive terms; a query made only of stop words is rejected."
                },
                "working_dir": {
                    "type": "string",
                    "description": "Only sessions whose working directory starts with this path (case-insensitive)."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_LIMIT,
                    "description": format!("Maximum number of matches to return (default {DEFAULT_LIMIT}).")
                },
                "max_per_session": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_MAX_PER_SESSION,
                    "description": format!("Maximum matches taken from one session (default {DEFAULT_MAX_PER_SESSION}, for result diversity).")
                },
                "include_current": {
                    "type": "boolean",
                    "description": "Include the active session. Defaults to false because it otherwise matches itself."
                },
                "include_tools": {
                    "type": "boolean",
                    "description": "Also search tool calls and tool results. Defaults to false to keep log noise out."
                },
                "role": {
                    "type": "string",
                    "enum": ["all", "user", "assistant", "summary"],
                    "description": "Restrict matches to one class of content (default all = user, assistant and summaries)."
                },
                "provider": {
                    "type": "string",
                    "description": "Only sessions whose provider name contains this substring (case-insensitive)."
                },
                "model": {
                    "type": "string",
                    "description": "Only sessions whose model id contains this substring (case-insensitive)."
                },
                "after": {
                    "type": "string",
                    "description": "Only matches at or after this date/time: `YYYY-MM-DD` (local) or RFC3339."
                },
                "before": {
                    "type": "string",
                    "description": "Only matches at or before this date/time: `YYYY-MM-DD` (local, end of day) or RFC3339."
                },
                "context": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": MAX_CONTEXT,
                    "description": format!("Neighbouring messages shown before and after each match (default {DEFAULT_CONTEXT}).")
                },
                "max_scan_sessions": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_MAX_SCAN_SESSIONS,
                    "description": format!("How many of the most recently updated session files to scan (default {DEFAULT_MAX_SCAN_SESSIONS}).")
                }
            },
            "required": ["query"]
        }),
        snippet: format!("Use to recall what happened in earlier sessions (decisions, commands, files). Searches saved {APP_NAME} session transcripts on disk."),
        prompt_guidelines: vec![
            format!("`session_search` only reads this machine's saved {APP_NAME} sessions; it cannot see other tools' histories."),
            "Queries are keyword based: prefer distinctive terms, and add `after` / `before` when you know roughly when it happened.".to_string(),
        ],
        constrained_sampling: false,
        render_shell: None,
        execution_mode: None,
        prepare_arguments: None,
        grammar_sampling: None,
    }
}

/// `session_search` 工具的执行体：解析参数 → 后台扫描会话 → 渲染结果文本。
async fn run_session_search(args: &Value, ctx: &ToolExecCtx) -> Result<ToolResult, ToolError> {
    let req = build_request(args, ctx)?;

    // 扫描可能要读上百份会话文件：交给阻塞线程池；无 tokio 运行时（单测）就地跑。
    let scan = {
        let req = req.clone();
        move || run_search(&req)
    };
    let report = match tokio::runtime::Handle::try_current() {
        Ok(handle) => handle
            .spawn_blocking(scan)
            .await
            .map_err(|e| ToolError(format!("session search task failed: {e}")))?,
        Err(_) => scan(),
    };

    Ok(ToolResult::text(render_report(&req.query, &report)))
}

/// 从工具参数构造归一化的检索请求；缺参、越界与不可解析的时间都返回 `ToolError`。
fn build_request(args: &Value, ctx: &ToolExecCtx) -> Result<Request, ToolError> {
    let params: Params = serde_json::from_value(args.clone())
        .map_err(|e| ToolError(format!("invalid `session_search` arguments: {e}")))?;

    let query = params.query.trim().to_string();
    if query.is_empty() {
        return Err(ToolError("`query` must not be empty.".to_string()));
    }

    let terms = text::terms(&query, MAX_TERMS);
    if terms.is_empty() {
        return Err(ToolError(format!(
            "`query` has no searchable keyword ({query:?} is made only of stop words); use a more distinctive term."
        )));
    }

    let role = match params.role.as_deref() {
        None => RoleFilter::All,
        Some(raw) => raw.parse::<RoleFilter>().map_err(|_| {
            ToolError(format!(
                "invalid `role` {raw:?}: expected one of all, user, assistant, summary"
            ))
        })?,
    };

    let include_tools = params.include_tools.unwrap_or(false);
    if role == RoleFilter::Summary && include_tools {
        return Err(ToolError(
            "`role: summary` only searches compaction and branch summaries; drop `include_tools`."
                .to_string(),
        ));
    }

    let current = if params.include_current.unwrap_or(false) {
        None
    } else {
        current_session_file(&ctx.cwd)
    };

    Ok(Request {
        query,
        terms,
        working_dir: params
            .working_dir
            .map(|d| d.trim().to_lowercase())
            .filter(|d| !d.is_empty()),
        limit: bounded("limit", params.limit, DEFAULT_LIMIT, 1, MAX_LIMIT)?,
        max_per_session: bounded(
            "max_per_session",
            params.max_per_session,
            DEFAULT_MAX_PER_SESSION,
            1,
            MAX_MAX_PER_SESSION,
        )?,
        include_tools,
        role,
        provider: params
            .provider
            .map(|p| p.trim().to_lowercase())
            .filter(|p| !p.is_empty()),
        model: params
            .model
            .map(|m| m.trim().to_lowercase())
            .filter(|m| !m.is_empty()),
        after_ms: parse_bound_param("after", params.after.as_deref(), false)?,
        before_ms: parse_bound_param("before", params.before.as_deref(), true)?,
        context: bounded("context", params.context, DEFAULT_CONTEXT, 0, MAX_CONTEXT)?,
        max_scan_sessions: bounded(
            "max_scan_sessions",
            params.max_scan_sessions,
            DEFAULT_MAX_SCAN_SESSIONS,
            1,
            MAX_MAX_SCAN_SESSIONS,
        )?,
        current,
        sessions_root: settings_manager::agent_dir().join("sessions"),
    })
}

/// `session_search` 的原始参数（除 `query` 外都可省）。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Params {
    /// 检索关键词（必填）。
    query: String,
    /// 只看工作目录以此路径开头的会话。
    working_dir: Option<String>,
    /// 返回的命中条数上限。
    limit: Option<usize>,
    /// 同一会话最多贡献的命中条数。
    max_per_session: Option<usize>,
    /// 是否把当前会话也纳入结果。
    include_current: Option<bool>,
    /// 是否检索工具调用与工具结果。
    include_tools: Option<bool>,
    /// 限定命中来自哪一类内容（`all` / `user` / `assistant` / `summary`）。
    role: Option<String>,
    /// 按会话最后生效的 provider 名做子串过滤。
    provider: Option<String>,
    /// 按会话最后生效的 model id 做子串过滤。
    model: Option<String>,
    /// 命中时间下界（`YYYY-MM-DD` 或 RFC3339）。
    after: Option<String>,
    /// 命中时间上界（`YYYY-MM-DD` 或 RFC3339）。
    before: Option<String>,
    /// 每条命中前后附带的消息条数。
    context: Option<usize>,
    /// 扫描的会话文件份数上限。
    max_scan_sessions: Option<usize>,
}

/// 校验一个可省参数的取值范围：缺省取 `default`，越界返回 `ToolError`。
fn bounded(
    name: &str,
    value: Option<usize>,
    default: usize,
    min: usize,
    max: usize,
) -> Result<usize, ToolError> {
    let Some(value) = value else {
        return Ok(default);
    };
    if value < min || value > max {
        return Err(ToolError(format!(
            "`{name}` must be between {min} and {max}, got {value}."
        )));
    }
    Ok(value)
}

/// 解析 `after` / `before` 参数；给了值却解析不出来时报错（静默忽略过滤条件更糟）。
fn parse_bound_param(
    name: &str,
    raw: Option<&str>,
    end_of_day: bool,
) -> Result<Option<i64>, ToolError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    parse_date_bound_ms(raw, end_of_day)
        .map(Some)
        .ok_or_else(|| {
            ToolError(format!(
                "invalid `{name}` {raw:?}: expected `YYYY-MM-DD` or an RFC3339 timestamp."
            ))
        })
}

/// `role` 参数的取值：限定命中来自哪一类内容。
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumString, IntoStaticStr)]
#[strum(serialize_all = "snake_case", ascii_case_insensitive)]
enum RoleFilter {
    /// 不限（默认）：user / assistant / 摘要全部参与。
    All,
    /// 只看用户消息。
    User,
    /// 只看助手消息（含 thinking 块）。
    Assistant,
    /// 只看压缩摘要与分支摘要。
    Summary,
}

impl RoleFilter {
    /// 该类别的命中是否通过 `role` 过滤。
    fn accepts(self, kind: Kind) -> bool {
        match self {
            RoleFilter::All => true,
            RoleFilter::User => kind == Kind::User,
            RoleFilter::Assistant => kind == Kind::Assistant,
            RoleFilter::Summary => matches!(kind, Kind::Compaction | Kind::BranchSummary),
        }
    }
}

/// 一条可检索内容的类别（决定展示标签与 `role` 过滤）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
enum Kind {
    /// 用户消息。
    User,
    /// 助手消息（正文与 thinking 块）。
    Assistant,
    /// 上下文压缩摘要。
    Compaction,
    /// 分支摘要。
    BranchSummary,
    /// 工具调用或工具结果（仅在 `include_tools` 时参与检索）。
    Tool,
}

/// 归一化后的检索请求。
#[derive(Debug, Clone)]
struct Request {
    /// 原始查询文本（只用于结果文案）。
    query: String,
    /// 查询词元（小写、去重、去停用词）。
    terms: Vec<String>,
    /// 工作目录前缀过滤（小写）。
    working_dir: Option<String>,
    /// 返回的命中条数上限。
    limit: usize,
    /// 同一会话最多贡献的命中条数。
    max_per_session: usize,
    /// 是否检索工具调用与工具结果。
    include_tools: bool,
    /// 命中类别过滤。
    role: RoleFilter,
    /// provider 名子串过滤（小写）。
    provider: Option<String>,
    /// model id 子串过滤（小写）。
    model: Option<String>,
    /// 命中时间下界（Unix 毫秒）。
    after_ms: Option<i64>,
    /// 命中时间上界（Unix 毫秒）。
    before_ms: Option<i64>,
    /// 每条命中前后附带的消息条数。
    context: usize,
    /// 扫描的会话文件份数上限。
    max_scan_sessions: usize,
    /// 要排除的会话文件（当前会话）；`None` = 不排除。
    current: Option<PathBuf>,
    /// 会话根目录（`agent_dir()/sessions`）。
    sessions_root: PathBuf,
}

/// 一份会话里可检索的一条内容。
#[derive(Debug, Clone)]
struct EntryText {
    /// 内容类别。
    kind: Kind,
    /// 该内容的时间戳（Unix 毫秒）。
    timestamp: i64,
    /// 已压成单行的可检索文本（超长时截到 [`MESSAGE_TEXT_CAP`]）。
    text: String,
}

/// 读盘并归一化后的一份会话。
#[derive(Debug, Clone)]
struct SessionScan {
    /// 会话文件路径。
    path: PathBuf,
    /// 用户指定的会话名；未命名时为 `None`。
    name: Option<String>,
    /// 会话的工作目录。
    cwd: String,
    /// 文件最后写入时间（Unix 毫秒），用于排序与时间预筛。
    modified_ms: i64,
    /// 会话最后生效的 provider 名。
    provider: Option<String>,
    /// 会话最后生效的 model id。
    model: Option<String>,
    /// 按写入顺序排列的可检索内容。
    entries: Vec<EntryText>,
}

/// 报告里的一条命中。
#[derive(Debug, Clone)]
struct Hit {
    /// 展示标签（`user` / `assistant` / `compaction` / `branch_summary` / `tool` / `session`）。
    label: &'static str,
    /// 命中时间（Unix 毫秒）。
    timestamp: i64,
    /// 围绕命中点的摘录。
    excerpt: String,
    /// 命中点之前的相邻消息（`context` 为 0 时为空）。
    before: Vec<String>,
    /// 命中点之后的相邻消息（`context` 为 0 时为空）。
    after: Vec<String>,
}

/// 报告里的一个会话分组。
#[derive(Debug, Clone)]
struct Group {
    /// 展示名，取自 [`session_title`]。
    name: String,
    /// 会话的工作目录。
    cwd: String,
    /// 会话最后生效的 provider 名。
    provider: Option<String>,
    /// 会话最后生效的 model id。
    model: Option<String>,
    /// 该会话的命中（已按得分降序）。
    hits: Vec<Hit>,
}

/// 一次检索的结果。
#[derive(Debug, Default)]
struct Report {
    /// 命中涉及的会话分组（按最佳命中得分降序）。
    groups: Vec<Group>,
    /// 实际扫描的会话文件份数。
    scanned: usize,
    /// 候选会话文件总数（`max_scan_sessions` 截断前）。
    available: usize,
    /// 返回的命中总条数。
    matches: usize,
}

/// 执行一次检索：列文件 → 逐份扫描打分 → 全局排序并截断。
fn run_search(req: &Request) -> Report {
    let mut report = Report::default();

    let all = session_files(&req.sessions_root);
    report.available = all.len();

    // 时间下界与「排除当前会话」都能在不读盘的前提下判掉：早于下界的文件整份都在下界之前。
    let files: Vec<PathBuf> = all
        .into_iter()
        .filter(|(_, path)| req.current.as_deref() != Some(path.as_path()))
        .filter(|(mtime, _)| req.after_ms.is_none_or(|after| *mtime >= after))
        .take(req.max_scan_sessions)
        .map(|(_, path)| path)
        .collect();
    report.scanned = files.len();

    // (得分, 分组下标, 组内命中下标)：只保留摘录，不把整份会话文本留在内存里。
    let mut ranked: Vec<(f64, usize, usize)> = Vec::new();

    for path in &files {
        let Some(scan) = read_session(path, req.include_tools) else {
            continue; // 打不开或需要修复的会话跳过：检索不是修复工具
        };
        if !session_passes_filters(&scan, req) {
            continue;
        }

        let scored = score_session(&scan, req);
        if scored.is_empty() {
            continue;
        }

        let group_index = report.groups.len();
        report.groups.push(Group {
            name: session_title(&scan),
            cwd: scan.cwd.clone(),
            provider: scan.provider.clone(),
            model: scan.model.clone(),
            hits: scored.iter().map(|(_, hit)| hit.clone()).collect(),
        });
        for (hit_index, (score, _)) in scored.iter().enumerate() {
            ranked.push((*score, group_index, hit_index));
        }
    }

    // 全局按得分降序（同分时新的在前），再按 `max_per_session` 与 `limit` 截断。
    ranked.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                report.groups[b.1].hits[b.2]
                    .timestamp
                    .cmp(&report.groups[a.1].hits[a.2].timestamp)
            })
    });

    // 组内只留被选中的命中，同时记住每个分组的最佳得分（分组顺序按它排）。
    let mut kept: Vec<Vec<Hit>> = vec![Vec::new(); report.groups.len()];
    let mut best: Vec<Option<f64>> = vec![None; report.groups.len()];
    let mut total = 0usize;
    for (score, group_index, hit_index) in ranked {
        if total >= req.limit || kept[group_index].len() >= req.max_per_session {
            continue;
        }
        if best[group_index].is_none() {
            best[group_index] = Some(score);
        }
        kept[group_index].push(report.groups[group_index].hits[hit_index].clone());
        total += 1;
    }

    let mut groups: Vec<(f64, Group)> = report
        .groups
        .into_iter()
        .zip(kept)
        .zip(best)
        .filter_map(|((mut group, hits), score)| {
            if hits.is_empty() {
                return None;
            }
            group.hits = hits;
            Some((score.unwrap_or(0.0), group))
        })
        .collect();
    groups.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    report.matches = total;
    report.groups = groups.into_iter().map(|(_, group)| group).collect();
    report
}

/// 会话级过滤：工作目录前缀、provider / model 子串，以及时间下界的粗筛。
///
/// 时间粗筛只是省掉没必要的读盘（命中仍按消息时间逐条过滤）：文件最后写入早于 `after`
/// 说明整份会话都在下界之前。上界不做粗筛——消息时间可能早于写入时间（导入/恢复的会话），
/// 用首条条目的时间做粗筛会漏掉真正在范围内的会话。
fn session_passes_filters(scan: &SessionScan, req: &Request) -> bool {
    if let Some(dir) = req.working_dir.as_deref()
        && !scan.cwd.to_lowercase().starts_with(dir)
    {
        return false;
    }
    if let Some(provider) = req.provider.as_deref()
        && !scan
            .provider
            .as_deref()
            .is_some_and(|p| p.to_lowercase().contains(provider))
    {
        return false;
    }
    if let Some(model) = req.model.as_deref()
        && !scan
            .model
            .as_deref()
            .is_some_and(|m| m.to_lowercase().contains(model))
    {
        return false;
    }
    if let Some(after) = req.after_ms
        && scan.modified_ms < after
    {
        return false;
    }
    true
}

/// 给一份会话里的内容打分，返回按得分降序、已截到 `max_per_session` 的命中。
fn score_session(scan: &SessionScan, req: &Request) -> Vec<(f64, Hit)> {
    let mut scored: Vec<(f64, Hit)> = Vec::new();

    // 会话名命中：名字是用户亲手写的，单独作为一条 `session` 结果。
    if req.role == RoleFilter::All
        && let Some(name) = scan.name.as_deref()
    {
        let score = score_text(&name.to_lowercase(), &req.terms);
        if score > 0.0 {
            scored.push((
                score * NAME_SCORE_MULTIPLIER,
                Hit {
                    label: LABEL_SESSION,
                    timestamp: scan.modified_ms,
                    excerpt: name.to_string(),
                    before: Vec::new(),
                    after: Vec::new(),
                },
            ));
        }
    }

    for (index, entry) in scan.entries.iter().enumerate() {
        if !req.role.accepts(entry.kind) {
            continue;
        }
        if req.after_ms.is_some_and(|after| entry.timestamp < after)
            || req.before_ms.is_some_and(|before| entry.timestamp > before)
        {
            continue;
        }
        let score = score_text(&entry.text.to_lowercase(), &req.terms);
        if score <= 0.0 {
            continue;
        }
        scored.push((
            score,
            Hit {
                label: entry.kind.into(),
                timestamp: entry.timestamp,
                excerpt: excerpt_containing(&entry.text, &req.terms, EXCERPT_CHARS),
                before: context_lines(scan, index, req.context, true),
                after: context_lines(scan, index, req.context, false),
            },
        ));
    }

    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.1.timestamp.cmp(&a.1.timestamp))
    });
    scored.truncate(req.max_per_session);
    scored
}

/// 命中点前后各 `count` 条相邻消息的单行展示（`count` 为 0 时返回空）。
fn context_lines(scan: &SessionScan, index: usize, count: usize, before: bool) -> Vec<String> {
    if count == 0 {
        return Vec::new();
    }
    let label = |entry: &EntryText| -> &'static str { entry.kind.into() };
    if before {
        let start = index.saturating_sub(count);
        scan.entries[start..index]
            .iter()
            .map(|entry| {
                format!(
                    "[{}] {}",
                    label(entry),
                    excerpt_containing(&entry.text, &[], CONTEXT_CHARS)
                )
            })
            .collect()
    } else {
        scan.entries
            .iter()
            .skip(index + 1)
            .take(count)
            .map(|entry| {
                format!(
                    "[{}] {}",
                    label(entry),
                    excerpt_containing(&entry.text, &[], CONTEXT_CHARS)
                )
            })
            .collect()
    }
}

/// 文本对一组词元的得分：各词元出现次数之和 + 命中词元种类的加权；一个都没命中为 0。
///
/// `haystack` 必须是已小写化的文本。
fn score_text(haystack: &str, terms: &[String]) -> f64 {
    let mut total = 0.0;
    let mut distinct = 0u32;
    for term in terms {
        let hits = haystack.matches(term.as_str()).count().min(MAX_TERM_HITS);
        if hits > 0 {
            distinct += 1;
            total += hits as f64;
        }
    }
    if distinct == 0 {
        return 0.0;
    }
    total + DISTINCT_TERM_BONUS * f64::from(distinct - 1)
}

/// 列 `agent_dir()/sessions` 下全部会话文件，按修改时间倒序（最新在前），带上修改时间毫秒。
fn session_files(root: &Path) -> Vec<(i64, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };

    let mut files: Vec<(u128, i64, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }

        for path in session_manager::list_sessions(&dir) {
            let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
            let mtime = modified.map(system_to_ms).unwrap_or(0);
            let nanos = modified
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            files.push((nanos, mtime as i64, path));
        }
    }

    files.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.2.cmp(&a.2)));
    files
        .into_iter()
        .map(|(_, mtime, path)| (mtime, path))
        .collect()
}

/// 会话文件的显示名（文件名去扩展名，即会话 id）。
fn session_stem(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string())
}

/// 会话在结果里的展示名：用户命名优先，其次首条用户消息（截到 [`TITLE_CHARS`]），
/// 最后退回会话 id——无名的会话用文件 id 做标题对模型毫无信息量。
fn session_title(scan: &SessionScan) -> String {
    if let Some(name) = scan.name.as_deref() {
        return name.to_string();
    }
    if let Some(first) = scan.entries.iter().find(|entry| entry.kind == Kind::User) {
        let title = excerpt_containing(&first.text, &[], TITLE_CHARS);
        if !title.is_empty() {
            return title;
        }
    }
    session_stem(&scan.path)
}

/// 判定当前会话文件：优先用 `on_session_start` / `on_session_switched` 记录的路径。
///
/// 记录缺失时（扩展在会话中途被启用）退回「当前 cwd 的会话目录里最新那份」——
/// 当前会话一直在追加写入，因此它通常就是最新的那份。
fn current_session_file(cwd: &str) -> Option<PathBuf> {
    if let Some(path) = lock().session_file.clone() {
        return Some(path);
    }
    let dir = session_manager::default_session_dir(cwd, &settings_manager::agent_dir());
    session_manager::list_sessions(&dir).into_iter().next()
}

/// 读一份会话文件并归一化成可检索内容；打不开或需要修复时返回 `None`。
///
/// `include_tools` 为假时**根本不构造**工具调用/结果的文本：工具日志是会话文件里体积最大
/// 的一块，跳过它能省掉大部分扫描开销。
fn read_session(path: &Path, include_tools: bool) -> Option<SessionScan> {
    let session = match Session::open_checked(&path.to_string_lossy()) {
        OpenSessionResult::Ok(session) => session,
        OpenSessionResult::Invalid(_) | OpenSessionResult::Repairable { .. } => return None,
    };

    let modified_ms = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .map(system_to_ms)
        .unwrap_or(0) as i64;

    let mut entries: Vec<EntryText> = Vec::new();
    let mut provider: Option<String> = None;
    let mut model: Option<String> = None;
    let mut budget = SESSION_TEXT_BUDGET;

    for entry in session.state.entries() {
        let (kind, timestamp, text) = match entry {
            Entry::ModelChange {
                provider: entry_provider,
                model_id,
                ..
            } => {
                provider = Some(entry_provider.clone());
                model = Some(model_id.clone());
                continue;
            }
            Entry::Message { base, message, .. } => {
                if message.provider.is_some() {
                    provider = message.provider.clone();
                }
                if message.model.is_some() {
                    model = message.model.clone();
                }
                let Some(kind) = message_kind(&message.role, include_tools) else {
                    continue;
                };
                let text = flatten_line(&message_text(message, include_tools));
                if text.is_empty() {
                    continue;
                }
                let timestamp = if message.timestamp > 0 {
                    message.timestamp as i64
                } else {
                    base.timestamp
                };
                (kind, timestamp, text)
            }
            Entry::Compaction { base, summary, .. } => {
                (Kind::Compaction, base.timestamp, flatten_line(summary))
            }
            Entry::BranchSummary { base, summary, .. } => {
                (Kind::BranchSummary, base.timestamp, flatten_line(summary))
            }
            _ => continue,
        };

        if text.is_empty() || budget == 0 {
            continue;
        }

        let text = truncate_chars(&text, MESSAGE_TEXT_CAP, None, false);
        budget = budget.saturating_sub(text.chars().count());
        entries.push(EntryText {
            kind,
            timestamp,
            text,
        });
    }

    Some(SessionScan {
        path: path.to_path_buf(),
        name: session.name.clone(),
        cwd: session.cwd.clone(),
        modified_ms,
        provider,
        model,
        entries,
    })
}

/// 消息 role → 可检索类别；未知 role、以及 `include_tools` 为假时的工具结果都返回 `None`。
fn message_kind(role: &str, include_tools: bool) -> Option<Kind> {
    match role {
        "user" => Some(Kind::User),
        "assistant" => Some(Kind::Assistant),
        "toolResult" => include_tools.then_some(Kind::Tool),
        _ => None,
    }
}

/// 拼一条消息的可检索文本：正文与 thinking 块按顺序拼接，工具调用/结果仅在
/// `include_tools` 时进（工具调用带上入参 JSON，便于按参数里的关键词回忆）。
///
/// `<system-reminder>` 开头的用户消息是注入的提醒而不是用户的话，返回空串（调用方会跳过）。
fn message_text(message: &AgentMessage, include_tools: bool) -> String {
    let mut parts: Vec<&str> = Vec::new();
    let mut owned: Vec<String> = Vec::new();

    for block in &message.content {
        match block {
            ContentBlock::Text { text, .. } => parts.push(text.as_str()),
            ContentBlock::Thinking {
                thinking, redacted, ..
            } => {
                if *redacted != Some(true) {
                    parts.push(thinking.as_str());
                }
            }
            ContentBlock::ToolCall {
                name, arguments, ..
            } => {
                if include_tools {
                    owned.push(format!("{name} {arguments}"));
                }
            }
            ContentBlock::Image { .. } => {}
        }
    }

    let mut text = parts.join(" ");
    if !owned.is_empty() {
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(&owned.join(" "));
    }

    // 注入的 system-reminder 不是用户的话：整条跳过，避免它命中用户查询里的常见词
    if message.role == "user" && text.trim_start().starts_with("<system-reminder>") {
        return String::new();
    }
    text
}

/// 把报告渲染成交给模型的文本。
fn render_report(query: &str, report: &Report) -> String {
    if report.matches == 0 {
        return format!("No matches for {query:?} ({}).", scan_scope(report));
    }

    let mut out = format!(
        "Found {} match{} in {} session{} ({}).",
        report.matches,
        if report.matches == 1 { "" } else { "es" },
        report.groups.len(),
        if report.groups.len() == 1 { "" } else { "s" },
        scan_scope(report),
    );

    for group in &report.groups {
        out.push_str("\n\n─── ");
        out.push_str(&group.name);
        out.push_str(" · ");
        out.push_str(&format_local(
            group.hits.first().map_or(0, |h| h.timestamp),
            "%Y-%m-%d %H:%M",
        ));
        if !group.cwd.is_empty() {
            out.push_str(" · ");
            out.push_str(&group.cwd);
        }
        if let Some(model) = group_model(group) {
            out.push_str(" · ");
            out.push_str(&model);
        }
        for hit in &group.hits {
            for line in &hit.before {
                out.push_str("\n  ↳ ");
                out.push_str(line);
            }
            out.push_str("\n  [");
            out.push_str(hit.label);
            out.push_str("] ");
            out.push_str(&hit.excerpt);
            for line in &hit.after {
                out.push_str("\n  ↳ ");
                out.push_str(line);
            }
        }
    }

    out
}

/// 报告头部的扫描范围说明。
fn scan_scope(report: &Report) -> String {
    let plural = |n: usize| if n == 1 { "" } else { "s" };
    if report.available > report.scanned {
        format!(
            "scanned the {} most recent of {} session files",
            report.scanned, report.available
        )
    } else {
        format!(
            "scanned {} session file{}",
            report.scanned,
            plural(report.scanned)
        )
    }
}

/// 会话的 `provider/model` 展示串；两者都没有时返回 `None`。
fn group_model(group: &Group) -> Option<String> {
    match (&group.provider, &group.model) {
        (Some(provider), Some(model)) => Some(format!("{provider}/{model}")),
        (Some(provider), None) => Some(provider.clone()),
        (None, Some(model)) => Some(model.clone()),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::AgentDirGuard;
    use serde_json::json;

    /// 只填 cwd 的测试上下文：`session_search` 不碰子代理与嵌套工具，其余字段给不可用占位。
    fn ctx(cwd: &str) -> ToolExecCtx {
        ToolExecCtx {
            cwd: cwd.to_string(),
            make_sub_agent: Arc::new(|_spec| Err("subagents unavailable in tests".to_string())),
            parent_abort: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            agent_id: None,
            depth: 0,
            parent_model: None,
            execute_tool: crate::core::extensions::unavailable_tool_exec(),
            parent_tool_call_id: None,
            nested_calls: Default::default(),
            script_tools: Default::default(),
            session_branch_entries: Default::default(),
            script_call: false,
        }
    }

    /// 检索请求的测试构造：只给必填项，其余取默认。
    fn request(query: &str, root: &Path) -> Request {
        Request {
            query: query.to_string(),
            terms: text::terms(query, MAX_TERMS),
            working_dir: None,
            limit: DEFAULT_LIMIT,
            max_per_session: DEFAULT_MAX_PER_SESSION,
            include_tools: false,
            role: RoleFilter::All,
            provider: None,
            model: None,
            after_ms: None,
            before_ms: None,
            context: DEFAULT_CONTEXT,
            max_scan_sessions: DEFAULT_MAX_SCAN_SESSIONS,
            current: None,
            sessions_root: root.to_path_buf(),
        }
    }

    /// 一条测试消息（`timestamp` 固定，避免断言依赖当前时间）。
    fn message(role: &str, text: &str) -> AgentMessage {
        AgentMessage {
            role: role.to_string(),
            content: vec![ContentBlock::Text {
                text: text.to_string(),
                text_signature: None,
            }],
            tool_call_id: None,
            tool_name: None,
            is_error: false,
            stop_reason: None,
            error_message: None,
            model: Some("test-model".to_string()),
            provider: Some("test-provider".to_string()),
            api: None,
            response_model: None,
            response_id: None,
            thinking_level: None,
            diagnostics: None,
            usage: None,
            deferred: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 1_700_000_000_000,
            duration_ms: None,
            details: None,
            citations: None,
            entry_id: None,
        }
    }

    /// 在 `dir` 下写一份会话（可带名字），返回文件路径。
    fn write_session(dir: &Path, name: Option<&str>, messages: &[(&str, &str)]) -> PathBuf {
        let mut session = Session::create("", Some(dir.to_path_buf()), true).unwrap();
        for (role, text) in messages {
            session.append_message(&message(role, text));
        }
        if let Some(name) = name {
            session.append_session_info(name);
        }
        session.get_session_file().unwrap().to_path_buf()
    }

    /// 建一个临时 agent_dir（用例内自清理），返回它的 `sessions` 根目录。
    fn temp_root(tag: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("prux-session-search-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn fork_project_reports_jcode_source() {
        let info = SessionSearch
            .fork_project()
            .expect("session-search 应声明 fork 来源");
        assert_eq!(info.plugin_name, "jcode");
        assert_eq!(info.plugin_version, "0.94.0");
        assert_eq!(info.url, "https://github.com/1jehuang/jcode");
    }

    #[test]
    fn tool_is_deferred_and_declares_required_query() {
        let tool = session_search_tool();
        assert_eq!(tool.name, TOOL_SESSION_SEARCH);
        assert_eq!(tool.exposure, ToolExposure::Deferred);
        assert_eq!(tool.parameters["required"], json!(["query"]));
        assert!(tool.annotations.read_only);
        assert_eq!(
            SessionSearch.prerequisite_tools(),
            vec![tool_search::TOOL_NAME.to_string()]
        );
    }

    #[test]
    fn stop_word_only_query_is_rejected() {
        let _ad = AgentDirGuard::temp();
        let err = build_request(&json!({ "query": "the and of" }), &ctx("/tmp")).unwrap_err();
        assert!(err.0.contains("no searchable keyword"), "{}", err.0);
    }

    #[test]
    fn empty_query_and_unknown_field_are_rejected() {
        let _ad = AgentDirGuard::temp();
        let err = build_request(&json!({ "query": "   " }), &ctx("/tmp")).unwrap_err();
        assert!(err.0.contains("must not be empty"), "{}", err.0);

        let err = build_request(&json!({ "query": "xyz", "nope": 1 }), &ctx("/tmp")).unwrap_err();
        assert!(
            err.0.contains("invalid `session_search` arguments"),
            "{}",
            err.0
        );
    }

    #[test]
    fn out_of_range_numbers_are_rejected() {
        let _ad = AgentDirGuard::temp();
        let err =
            build_request(&json!({ "query": "xyz", "limit": 999 }), &ctx("/tmp")).unwrap_err();
        assert!(err.0.contains("`limit` must be between"), "{}", err.0);

        let err =
            build_request(&json!({ "query": "xyz", "context": 9 }), &ctx("/tmp")).unwrap_err();
        assert!(err.0.contains("`context` must be between"), "{}", err.0);
    }

    #[test]
    fn invalid_role_and_time_bounds_are_rejected() {
        let _ad = AgentDirGuard::temp();
        let err =
            build_request(&json!({ "query": "xyz", "role": "tools" }), &ctx("/tmp")).unwrap_err();
        assert!(err.0.contains("invalid `role`"), "{}", err.0);

        let err = build_request(
            &json!({ "query": "xyz", "after": "yesterday" }),
            &ctx("/tmp"),
        )
        .unwrap_err();
        assert!(err.0.contains("invalid `after`"), "{}", err.0);
    }

    #[test]
    fn summary_role_rejects_include_tools() {
        let _ad = AgentDirGuard::temp();
        let err = build_request(
            &json!({ "query": "xyz", "role": "summary", "include_tools": true }),
            &ctx("/tmp"),
        )
        .unwrap_err();
        assert!(err.0.contains("drop `include_tools`"), "{}", err.0);
    }

    #[test]
    fn build_request_lowercases_the_substring_filters() {
        let _ad = AgentDirGuard::temp();
        let req = build_request(
            &json!({ "query": "xyz", "provider": "Anthropic", "model": "Sonnet", "working_dir": "/Data/Code" }),
            &ctx("/tmp"),
        )
        .unwrap();
        assert_eq!(req.provider.as_deref(), Some("anthropic"));
        assert_eq!(req.model.as_deref(), Some("sonnet"));
        assert_eq!(req.working_dir.as_deref(), Some("/data/code"));
    }

    #[test]
    fn score_weights_distinct_terms_over_repetition() {
        let terms = text::terms("alpha beta", MAX_TERMS);
        let one_term = score_text("alpha alpha alpha", &terms);
        let two_terms = score_text("alpha beta", &terms);
        assert!(two_terms > one_term, "{two_terms} 应高于 {one_term}");
        assert_eq!(score_text("nothing here", &terms), 0.0);
        // 同一个词重复到上限之后不再加分
        let many = score_text(&"alpha ".repeat(50), &terms);
        let capped = score_text(&"alpha ".repeat(MAX_TERM_HITS), &terms);
        assert_eq!(many, capped);
    }

    #[test]
    fn score_is_case_insensitive_through_lowercasing() {
        let terms = text::terms("CacheWatch", MAX_TERMS);
        assert!(score_text(&"cachewatch happened".to_lowercase(), &terms) > 0.0);
    }

    #[test]
    fn chinese_query_finds_chinese_content() {
        let root = temp_root("cjk-search");
        let dir = root.join("--proj--");
        std::fs::create_dir_all(&dir).unwrap();
        write_session(
            &dir,
            None,
            &[("user", "上次我们讨论过缓存失效的问题，结论是加 TTL")],
        );
        write_session(&dir, None, &[("user", "unrelated content")]);

        let report = run_search(&request("如何修复缓存失效？", &root));
        assert_eq!(report.matches, 1, "中文长句查询应命中：{report:?}");
        assert!(
            report.groups[0].hits[0].excerpt.contains("缓存失效"),
            "{:?}",
            report.groups[0].hits[0].excerpt
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn role_filter_accepts_expected_kinds() {
        assert!(RoleFilter::All.accepts(Kind::Tool));
        assert!(RoleFilter::User.accepts(Kind::User));
        assert!(!RoleFilter::User.accepts(Kind::Assistant));
        assert!(RoleFilter::Assistant.accepts(Kind::Assistant));
        assert!(RoleFilter::Summary.accepts(Kind::Compaction));
        assert!(RoleFilter::Summary.accepts(Kind::BranchSummary));
        assert!(!RoleFilter::Summary.accepts(Kind::User));
        assert_eq!(<&str>::from(Kind::BranchSummary), "branch_summary");
    }

    #[test]
    fn message_text_skips_tools_and_system_reminders() {
        let with_tool = AgentMessage {
            content: vec![
                ContentBlock::Text {
                    text: "answer".to_string(),
                    text_signature: None,
                },
                ContentBlock::ToolCall {
                    id: "t1".to_string(),
                    name: "bash".to_string(),
                    arguments: json!({ "command": "cargo test" }),
                    thought_signature: None,
                    namespace: None,
                    arguments_error: None,
                },
            ],
            ..message("assistant", "")
        };
        assert_eq!(message_text(&with_tool, false).trim(), "answer");
        assert!(message_text(&with_tool, true).contains("cargo test"));

        let reminder = message("user", "<system-reminder>ignore me</system-reminder>");
        assert!(message_text(&reminder, false).is_empty());
    }

    #[test]
    fn read_session_collects_roles_and_summaries() {
        let root = temp_root("read");
        let dir = root.join("--proj--");
        std::fs::create_dir_all(&dir).unwrap();
        let path = write_session(
            &dir,
            Some("my-session"),
            &[
                ("user", "find the cache bug"),
                ("assistant", "the fix was X"),
            ],
        );

        let scan = read_session(&path, false).expect("会话应能读出");
        assert_eq!(scan.name.as_deref(), Some("my-session"));
        assert_eq!(scan.provider.as_deref(), Some("test-provider"));
        assert_eq!(scan.model.as_deref(), Some("test-model"));
        assert_eq!(scan.entries.len(), 2);
        assert_eq!(scan.entries[0].kind, Kind::User);
        assert_eq!(scan.entries[1].kind, Kind::Assistant);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn unreadable_session_is_skipped() {
        let root = temp_root("broken");
        let dir = root.join("--proj--");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("broken.jsonl"), b"{not json\n").unwrap();

        assert!(read_session(&dir.join("broken.jsonl"), false).is_none());
        let report = run_search(&request("anything", &root));
        assert_eq!(report.matches, 0);
        assert_eq!(report.scanned, 1, "坏文件仍算进扫描份数");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn search_groups_hits_by_session() {
        let root = temp_root("group");
        let dir = root.join("--proj--");
        std::fs::create_dir_all(&dir).unwrap();
        write_session(&dir, Some("alpha"), &[("user", "the registry lock bug")]);
        write_session(&dir, Some("beta"), &[("user", "unrelated content")]);

        let report = run_search(&request("registry lock", &root));
        assert_eq!(report.matches, 1);
        assert_eq!(report.groups.len(), 1);
        assert_eq!(report.groups[0].name, "alpha");
        assert_eq!(report.groups[0].hits[0].label, "user");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn current_session_is_excluded_by_default() {
        let root = temp_root("current");
        let dir = root.join("--proj--");
        std::fs::create_dir_all(&dir).unwrap();
        let current = write_session(&dir, Some("current"), &[("user", "registry lock")]);
        write_session(&dir, Some("older"), &[("user", "registry lock")]);

        let mut req = request("registry lock", &root);
        req.current = Some(current);
        let report = run_search(&req);
        assert_eq!(report.groups.len(), 1);
        assert_eq!(report.groups[0].name, "older");

        // 显式要求包含当前会话时它回来了
        req.current = None;
        assert_eq!(run_search(&req).groups.len(), 2);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn session_name_match_is_reported_as_a_session_hit() {
        let root = temp_root("name");
        let dir = root.join("--proj--");
        std::fs::create_dir_all(&dir).unwrap();
        write_session(&dir, Some("cache-watch design"), &[("user", "unrelated")]);

        let report = run_search(&request("cache-watch", &root));
        assert_eq!(report.matches, 1);
        assert_eq!(report.groups[0].hits[0].label, LABEL_SESSION);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn working_dir_provider_and_model_filters_apply() {
        let root = temp_root("filter");
        let dir = root.join("--proj--");
        std::fs::create_dir_all(&dir).unwrap();
        let path = write_session(&dir, Some("s"), &[("user", "needle")]);

        let mut req = request("needle", &root);
        req.working_dir = Some("/definitely/elsewhere".to_string());
        assert_eq!(run_search(&req).matches, 0);

        req.working_dir = None;
        req.provider = Some("test-provider".to_string());
        assert_eq!(run_search(&req).matches, 1);

        req.provider = Some("openai".to_string());
        assert_eq!(run_search(&req).matches, 0);

        req.provider = None;
        req.model = Some("other".to_string());
        assert_eq!(run_search(&req).matches, 0);

        std::fs::remove_dir_all(&root).ok();
        drop(path);
    }

    #[test]
    fn limit_and_max_per_session_cap_the_result() {
        let root = temp_root("limit");
        let dir = root.join("--proj--");
        std::fs::create_dir_all(&dir).unwrap();
        write_session(
            &dir,
            Some("one"),
            &[("user", "needle one"), ("assistant", "needle two")],
        );
        write_session(&dir, Some("two"), &[("user", "needle three")]);

        let mut req = request("needle", &root);
        req.max_per_session = MAX_MAX_PER_SESSION;
        assert_eq!(run_search(&req).matches, 3);

        req.limit = 2;
        assert_eq!(run_search(&req).matches, 2);

        req.limit = DEFAULT_LIMIT;
        req.max_per_session = 1;
        let report = run_search(&req);
        assert_eq!(report.matches, 2, "每个会话只出 1 条");
        assert_eq!(report.groups.len(), 2);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn scan_session_cap_bounds_the_scan() {
        let root = temp_root("scan-cap");
        let dir = root.join("--proj--");
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..4 {
            write_session(&dir, Some(&format!("s{i}")), &[("user", "needle")]);
        }

        let mut req = request("needle", &root);
        req.max_scan_sessions = 2;
        let report = run_search(&req);
        assert_eq!(report.available, 4);
        assert_eq!(report.scanned, 2);
        assert_eq!(report.matches, 2);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn context_lines_surround_the_hit() {
        let root = temp_root("context");
        let dir = root.join("--proj--");
        std::fs::create_dir_all(&dir).unwrap();
        write_session(
            &dir,
            Some("s"),
            &[
                ("user", "first"),
                ("assistant", "second"),
                ("user", "needle here"),
                ("assistant", "fourth"),
            ],
        );

        let mut req = request("needle", &root);
        req.context = 1;
        let report = run_search(&req);
        let hit = &report.groups[0].hits[0];
        assert_eq!(hit.before.len(), 1);
        assert!(
            hit.before[0].starts_with("[assistant] second"),
            "{:?}",
            hit.before
        );
        assert_eq!(hit.after.len(), 1);
        assert!(
            hit.after[0].starts_with("[assistant] fourth"),
            "{:?}",
            hit.after
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn role_filter_restricts_hits() {
        let root = temp_root("role");
        let dir = root.join("--proj--");
        std::fs::create_dir_all(&dir).unwrap();
        write_session(
            &dir,
            Some("s"),
            &[
                ("user", "needle from user"),
                ("assistant", "needle from assistant"),
            ],
        );

        let mut req = request("needle", &root);
        req.max_per_session = MAX_MAX_PER_SESSION;
        req.role = RoleFilter::User;
        let report = run_search(&req);
        assert_eq!(report.matches, 1);
        assert_eq!(report.groups[0].hits[0].label, "user");

        req.role = RoleFilter::Assistant;
        assert_eq!(run_search(&req).groups[0].hits[0].label, "assistant");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn time_bounds_filter_hits() {
        let root = temp_root("time");
        let dir = root.join("--proj--");
        std::fs::create_dir_all(&dir).unwrap();
        write_session(&dir, Some("s"), &[("user", "needle")]);

        // 测试消息的时间戳固定为 1_700_000_000_000
        let mut req = request("needle", &root);
        req.after_ms = Some(1_700_000_001_000);
        assert_eq!(run_search(&req).matches, 0, "命中早于下界应被排掉");

        req.after_ms = Some(1_699_999_999_000);
        assert_eq!(run_search(&req).matches, 1);

        req.after_ms = None;
        req.before_ms = Some(1_699_999_999_000);
        assert_eq!(run_search(&req).matches, 0, "命中晚于上界应被排掉");

        req.before_ms = Some(1_700_000_001_000);
        assert_eq!(run_search(&req).matches, 1);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn unknown_roles_are_not_searchable() {
        assert_eq!(message_kind("user", false), Some(Kind::User));
        assert_eq!(message_kind("assistant", false), Some(Kind::Assistant));
        assert_eq!(message_kind("toolResult", false), None);
        assert_eq!(message_kind("toolResult", true), Some(Kind::Tool));
        assert_eq!(message_kind("system", true), None);
    }

    #[test]
    fn unnamed_session_uses_its_first_user_message_as_title() {
        let root = temp_root("title");
        let dir = root.join("--proj--");
        std::fs::create_dir_all(&dir).unwrap();
        write_session(
            &dir,
            None,
            &[("user", "fix the registry lock"), ("assistant", "ok")],
        );

        let report = run_search(&request("registry lock", &root));
        assert_eq!(report.groups[0].name, "fix the registry lock");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn render_reports_groups_hits_and_no_match() {
        let root = temp_root("render");
        let dir = root.join("--proj--");
        std::fs::create_dir_all(&dir).unwrap();
        write_session(&dir, Some("my-session"), &[("user", "the needle is here")]);

        let report = run_search(&request("needle", &root));
        let text = render_report("needle", &report);
        assert!(text.starts_with("Found 1 match in 1 session"), "{text}");
        assert!(text.contains("─── my-session"), "{text}");
        assert!(text.contains("[user] the needle is here"), "{text}");
        assert!(text.contains("test-provider/test-model"), "{text}");

        let empty = render_report("zzz-nomatch", &run_search(&request("zzz-nomatch", &root)));
        assert!(
            empty.starts_with("No matches for \"zzz-nomatch\""),
            "{empty}"
        );
        assert!(empty.contains("(scanned 1 session file)"), "{empty}");

        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn run_session_search_reports_matches_end_to_end() {
        let _ad = AgentDirGuard::temp();
        let root = settings_manager::agent_dir()
            .join("sessions")
            .join("--proj--");
        std::fs::create_dir_all(&root).unwrap();
        write_session(&root, Some("e2e"), &[("user", "e2e needle content")]);

        let res = run_session_search(&json!({ "query": "e2e needle" }), &ctx("/tmp"))
            .await
            .unwrap();
        assert!(res.text.contains("─── e2e"), "{}", res.text);
        assert!(res.text.contains("e2e needle content"), "{}", res.text);
    }

    #[tokio::test]
    async fn unknown_tool_name_is_rejected() {
        let err = SessionSearch
            .execute_tool_async("nope".to_string(), json!({}), ctx("/tmp"))
            .await
            .unwrap_err();
        assert!(err.0.contains("unknown tool: nope"), "{}", err.0);
    }

    #[test]
    fn session_file_state_is_used_when_known() {
        let _ad = AgentDirGuard::temp();
        let root = settings_manager::agent_dir()
            .join("sessions")
            .join("--cwd--");
        std::fs::create_dir_all(&root).unwrap();
        let path = write_session(&root, None, &[("user", "x")]);

        remember_session(Some(path.clone()));
        assert_eq!(current_session_file("/cwd"), Some(path.clone()));

        // 记录缺失时退回「当前 cwd 会话目录里最新那份」
        remember_session(None);
        assert_eq!(current_session_file("/cwd"), Some(path));
        assert_eq!(current_session_file("/nowhere"), None);
    }

    #[test]
    fn enabling_clears_the_recorded_session() {
        let _ad = AgentDirGuard::temp();
        remember_session(Some(PathBuf::from("/tmp/stale.jsonl")));
        SessionSearch.on_enabled_changed(true);
        assert!(lock().session_file.is_none(), "启用时应清掉可能过期的记录");
    }
}
