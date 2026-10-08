//! 子代理注册表、并发池、排队、通知投递与 resume。
//!
//! 全部状态是**进程级内存态**：
//! - `/new` `/resume` `/import` `/fork` 切换会话 → [`reset_all`]（全部中止并清空）
//! - 扩展被禁用 → [`reset_all`]
//!
//! 并发语义：后台池上限取配置 `max_concurrent`，`queued` 不占额度；
//! 前台额度取配置 `foreground_max_concurrent`（`0` = 不限），两个池**刻意分开**。
//! 配置缓存见 [`config`] 模块，面板变更后调 [`reload_config`] 刷新。

use super::{
    EXT,
    config::{self, ConfigKey, JoinMode, SubagentConfig},
    events, memory, mention,
    model::{self, ScopeVerdict},
    nested::{self, NESTED_TOOL_NAMES},
    notify::{self, CARD_TYPE},
    output_file,
    prompt::{self, SUB_AGENT_BRIDGE},
    session,
    types::{AgentRecord, AgentStatus, AgentType, PromptMode, SpawnRequest, UsageTotals},
    util_notify, widget, workflow,
    worktree::{self, Worktree},
};
use crate::{
    core::{
        extensions::{
            DockLine, ExtensionUiRequest, SubAgentControls, SubAgentEventSink, SubAgentRunner,
            SubAgentSpec, ToolExecCtx, UiNotifyLevel, is_nested_call_event, request_show_dock,
            request_ui,
        },
        provider::{AgentMessage, Usage},
    },
    extensions::util::next_id,
    utils::time::now_ms,
};
use futures_util::future::{Either, select};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::PathBuf,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::watch::Sender;

/// graceful max_turns 的宽限轮数。状态推断（`steered` vs `aborted`）依赖它。
pub const GRACE_TURNS: u32 = 5;

/// 保留的已终结记录上限（超出按插入顺序淘汰最旧的）。
const MAX_FINISHED_RECORDS: usize = 50;

/// 前台子代理轮询父级取消信号的间隔。
const PARENT_ABORT_POLL: Duration = Duration::from_millis(20);

/// 待启动（或运行中）的 spawn 描述。
struct PendingSpawn {
    /// spawn 时的请求（排队/启动时复用）。
    req: SpawnRequest,
    /// 解析后的类型定义。
    ty: AgentType,
    /// 完成信号发送端（`get_subagent_result(wait: true)` 用；排队期间也可等待）
    done_tx: Option<Sender<bool>>,
    /// 排队期间收到的 steer，启动时一并注入
    pending_steer: Vec<String>,
}

/// 子代理注册表、后台/前台并发计数、排队与通知批处理的进程级总状态。
#[derive(Default)]
struct State {
    /// 全部记录（id → 记录）。
    records: HashMap<String, AgentRecord>,
    /// 插入顺序（列表展示的稳定序）
    order: Vec<String>,
    /// 待启动的 spawn（id → 描述）。
    pending: HashMap<String, PendingSpawn>,
    /// 正在运行的后台任务数
    running_bg: usize,
    /// 正在运行的前台任务数（受 `foreground_max_concurrent` 约束）
    running_fg: usize,
    /// 后台排队等额度的记录 id（FIFO）。
    queue: VecDeque<String>,
    /// 告警去重表（键为 `来源|原因`）
    warned: HashSet<String>,
    /// 待合并通知的已完成工作（join 批处理；见 [`flush_batch`]）
    batch: Vec<BatchItem>,
    /// 批处理代际：窗口到期任务只在代际未变时投递（新批次使旧定时器失效）
    batch_generation: u64,
    /// 已终结记录的 dock 隐藏水位：`ended_ms <= 本值` 的终态记录不再进 dock。
    /// 用户开新回合（且没有在跑的活儿）时抬到当前时刻——见 [`dismiss_finished`]。
    /// 记录本身保留（`/agents` 面板 / `@handle` resume 不受影响）。
    dismissed_before_ms: u64,
    /// 配置缓存（惰性载入；路径或 mtime 变化时自动失效，见 [`config`]）
    cfg: SubagentConfig,
    /// 缓存对应的两层（路径, mtime）；路径参与判键以便测试的 agent_dir/cwd override 隔离
    cfg_key: ConfigKey,

    /// 测试：把配置钉住，`config()` 不再因别人改了文件（或**别的用例的 agent_dir override**）
    /// 而重载——那些重载来自上个用例遗留的后台任务，会把当前用例刚设的值清成缺省。
    #[cfg(test)]
    cfg_pinned: bool,
    /// 钉住配置的那个线程（只有它能改配置；别的线程的重载一律丢弃）
    #[cfg(test)]
    cfg_pin_owner: Option<std::thread::ThreadId>,
}

/// 进程级注册表 / 并发池 / 通知队列（全进程唯一）。
fn state() -> &'static Mutex<State> {
    /// 进程内唯一的注册表实例，所有子代理操作都经由它加锁访问。
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(State::default()))
}

/// 加锁访问进程级注册表，在锁内以 `&mut State` 执行 `f`（锁中毒时沿用内部状态）。
fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> R {
    let mut guard = state().lock().unwrap_or_else(|e| e.into_inner());
    f(&mut guard)
}

/// 当前配置快照（含面板可调项）。
///
/// 配置在进程内缓存，只做一次 `stat` 判断外部改动；widget 每帧读取的代价因此恒定。
pub fn config() -> SubagentConfig {
    // 测试里把缓存钉住：上个用例遗留的后台任务（定时扫描、工作流收尾…）会调这里，
    // 而它们的 agent_dir override 与本次不同 → 判定缓存失效 → 重载 → 把本次刚设的配置清成缺省。
    #[cfg(test)]
    if with_state(|st| st.cfg_pinned && st.cfg_pin_owner != Some(std::thread::current().id())) {
        return with_state(|st| st.cfg.clone());
    }

    let key = config::config_key();
    with_state(|st| {
        if st.cfg_key != key {
            st.cfg = config::load_config();
            st.cfg_key = key;
        }
        st.cfg.clone()
    })
}

/// 强制重读配置（面板写盘后调用；文件不可读时回退缺省）。
pub fn reload_config() {
    let cfg = config::load_config();
    let key = config::config_key();

    // 测试：别的线程在别人钉住配置时重载 → 丢弃。不然上个用例遗留的任务/用例会把
    // 当前用例刚设的配置清成缺省（agent_dir 是 thread-local override，键必然"变化"）。
    #[cfg(test)]
    if with_state(|st| st.cfg_pinned && st.cfg_pin_owner != Some(std::thread::current().id())) {
        return;
    }

    with_state(|st| {
        st.cfg = cfg;
        st.cfg_key = key;
    });
}

/// 分发结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dispatched {
    /// 代理 id（`resume` 时沿用被续跑代理的 id）
    pub id: String,
    /// 是否后台执行。
    pub background: bool,
    /// 后台且因额度已满而排队
    pub queued: bool,
}

/// 由类型定义构造一个后台 spawn 请求（mention / UI 续跑共用；
/// 无调用参数，frontmatter 字段全量透传）。
pub(crate) fn request_for_type(
    ty: &AgentType,
    prompt: String,
    description: String,
    resume: Option<String>,
) -> SpawnRequest {
    let cfg = config();
    let mut req = SpawnRequest::from_type(ty, &cfg, prompt, description);
    req.resume = resume;
    req
}

/// 按 spec 物化子代理（薄包装，统一错误文案）。
pub(crate) fn materialize(
    ctx: &ToolExecCtx,
    spec: SubAgentSpec,
) -> Result<Box<dyn SubAgentRunner>, String> {
    (ctx.make_sub_agent)(spec)
}

/// 由类型定义与请求构造 [`SubAgentSpec`]（frontmatter 权威性已在请求构造时应用）。
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_spec(
    ty: &AgentType,
    req: &SpawnRequest,
    cwd: &str,
    events: Option<SubAgentEventSink>,
    resume_session_path: Option<String>,
    max_depth: u32, // 嵌套深度上限（由调用方从配置读；做成参数是为了让本函数不碰全局配置）
    default_max_turns: u32, // 未显式给 max_turns 时的默认上限（0 = 不限）与收尾宽限轮数（同样由调用方传入）
    grace_turns: u32,
) -> SubAgentSpec {
    let tag = prompt::active_agent_tag(&ty.name);
    let skills_section = preload_skills_section(ty, &req.skills, cwd);
    let (mut system_prompt, mut append_system_prompt, clear_context_files) = build_prompt(ty, &tag);

    append_skills_section(
        &mut system_prompt,
        &mut append_system_prompt,
        &skills_section,
    );

    let mut tools = req.tools.clone();
    apply_delegation_tools(&mut tools, ty, req, max_depth);
    apply_memory(&mut tools, &mut append_system_prompt, ty, req, cwd);

    SubAgentSpec {
        agent_type: ty.name.clone(),
        system_prompt,
        append_system_prompt,
        clear_context_files,
        model: req.model.clone(),
        thinking: req.thinking.clone(),
        tools,
        max_turns: req
            .max_turns
            .or((default_max_turns > 0).then_some(default_max_turns)),
        grace_turns: Some(grace_turns),
        inherit_context: req.inherit_context,
        context_messages: None,
        cwd: None,
        persist_session: req.persist_session,
        resume_session_path,
        session_parent_id: None,
        session_dir: req.session_dir.clone().map(PathBuf::from),
        name: req.name.clone(),
        agent_id: None, // 身份：核心把它带进 child 的工具上下文，扩展据此限深与限属主。
        depth: req.depth,
        events,
        isolated: req.isolated,
        disallowed_tools: req.disallowed_tools.clone().unwrap_or_default(),
        extensions: req.extensions.clone(),
        exclude_extensions: req.exclude_extensions.clone(),
        clear_skills: req.clear_skills,
        injected_tools: req.injected_tools.clone(),
    }
}

/// 技能预载：`skills: [names]` 把正文注入 child（`skills: false` 走 spec.clear_skills）。
/// 找不到的技能不阻断派发，只告警一次。
fn preload_skills_section(ty: &AgentType, skills: &[String], cwd: &str) -> String {
    let (section, missing) = prompt::preload_skills(skills, cwd);

    for name in missing {
        let key = format!("skill-preload|{name}");
        if note_warning(&key) {
            util_notify(
                &format!(
                    "agent type {:?}: preloaded skill {name:?} was not found",
                    ty.name
                ),
                UiNotifyLevel::Warning,
            );
        }
    }
    section
}

/// 按 `prompt_mode` 组装系统提示：返回 `(replace, append, clear_context_files)`。
fn build_prompt(ty: &AgentType, tag: &str) -> (Option<String>, Option<String>, bool) {
    match ty.prompt_mode {
        PromptMode::Replace => {
            let body = ty.system_prompt.trim();
            let text = if body.is_empty() {
                tag.to_string()
            } else {
                format!("{tag}\n\n{body}")
            };
            (Some(text), None, true) // replace 模式不继承项目指令
        }
        PromptMode::Append => {
            let mut text = format!("{}\n\n{}", SUB_AGENT_BRIDGE, tag);
            let body = ty.system_prompt.trim();

            if !body.is_empty() {
                text.push_str(&format!(
                    "\n\n<agent_instructions>\n{body}\n</agent_instructions>"
                ));
            }
            (None, Some(text), false)
        }
    }
}

/// 技能正文挂到系统提示尾部（replace 挂 prompt，append 挂 append 槽）。
fn append_skills_section(
    system_prompt: &mut Option<String>,
    append_system_prompt: &mut Option<String>,
    section: &str,
) {
    if section.is_empty() {
        return;
    }

    match system_prompt.as_mut() {
        Some(prompt) => prompt.push_str(&format!("\n\n{section}")),
        None => {
            let slot = append_system_prompt.get_or_insert_with(String::new);
            slot.push_str(&format!("\n\n{section}"));
        }
    }
}

/// 嵌套委派：**opt-in**（agent 文件写了 `allowed_subagents` 才拿得到委派工具）
/// 且这一层还能再派一代时，把编排工具**显式写进它的 tools 白名单**
/// （核心的递归防护默认剔除这些名字，只放行 spec 明确列出的那些）。
/// `isolated` 下不给（即使写了`!options.isolated`也不给）。
fn apply_delegation_tools(
    tools: &mut Option<Vec<String>>,
    ty: &AgentType,
    req: &SpawnRequest,
    max_depth: u32,
) {
    if ty.allowed_subagents.is_some()
        && !req.isolated
        && nested::nesting_allowed(req.depth, max_depth)
    {
        let list = tools.get_or_insert_with(Vec::new);
        for name in NESTED_TOOL_NAMES {
            if !list.iter().any(|t| t == name) {
                list.push((*name).to_string());
            }
        }
    }
}

/// 持久记忆：frontmatter `memory` → 记忆块注入 append 提示 + 补 read/write/edit。
/// 未受信 / 非法名等失败不阻断派发，只告警一次。
fn apply_memory(
    tools: &mut Option<Vec<String>>,
    append_system_prompt: &mut Option<String>,
    ty: &AgentType,
    req: &SpawnRequest,
    cwd: &str,
) {
    let Some(scope) = ty.memory else {
        return;
    };

    let disallowed = req.disallowed_tools.clone().unwrap_or_default();

    match memory::plan(&ty.name, scope, cwd, tools, &disallowed) {
        Ok(plan) => {
            memory::apply_tools(tools, &plan.add_tools);

            let slot = append_system_prompt.get_or_insert_with(String::new);
            if !slot.is_empty() {
                slot.push_str("\n\n");
            }
            slot.push_str(&plan.block);
        }
        Err(e) => {
            let key = format!("memory|{}|{e}", ty.name);
            if note_warning(&key) {
                util_notify(
                    &format!("agent type {:?}: persistent memory skipped — {e}", ty.name),
                    UiNotifyLevel::Warning,
                );
            }
        }
    }
}

/// 建 `.output` 转写文件（`output_transcript` 开启时）并返回其路径。
fn open_output_file(cwd: &str, id: &str) -> Option<PathBuf> {
    let session = session::session_key();
    let meta = serde_json::json!({
        "type": "agent_start",
        "id": id,
        "cwd": cwd,
        "startedAtEpochMs": now_ms(),
    });
    output_file::create_output_file(cwd, session.as_deref(), id, &meta)
}

/// 子代理开销池（`report_usage` 开启时）：
/// 每个子代理的 assistant usage 记一份，挂到**下一个**真实工具结果上。
fn usage_pool() -> &'static Mutex<Option<Usage>> {
    /// 尚未挂到工具结果上的累计用量，由下一次真实工具结果取走。
    static P: OnceLock<Mutex<Option<Usage>>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(None))
}

/// 把一份子代理 usage 累加进开销池（池中已有条目则逐字段饱和相加，否则新建）。
pub(crate) fn pool_usage(u: Usage) {
    let mut p = usage_pool().lock().unwrap_or_else(|e| e.into_inner());
    match p.as_mut() {
        Some(acc) => {
            acc.input = acc.input.saturating_add(u.input);
            acc.output = acc.output.saturating_add(u.output);
            acc.cache_read = acc.cache_read.saturating_add(u.cache_read);
            acc.cache_write = acc.cache_write.saturating_add(u.cache_write);
            acc.total_tokens = acc.total_tokens.saturating_add(u.total_tokens);
            acc.cost.input += u.cost.input;
            acc.cost.output += u.cost.output;
            acc.cost.cache_read += u.cost.cache_read;
            acc.cost.cache_write += u.cost.cache_write;
            acc.cost.total += u.cost.total;
        }
        None => *p = Some(u),
    }
}

/// 取出并清空开销池（无开销时返回 None）。
pub fn drain_pooled_usage() -> Option<Usage> {
    usage_pool()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
}

/// 清空开销池（会话切换/禁用）。
pub fn reset_usage_pool() {
    *usage_pool().lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// 每 child 独立事件流：累加轮数/工具调用/用量并攒转写，并跟踪当前工具。
///
/// 只关心低频事件（`turn_end` / `message_end` / `tool_execution_start` / `tool_execution_end`），
/// 高频的 `message_update`（每个流式 delta）直接丢弃。
fn make_sink(id: &str, output: Option<PathBuf>) -> SubAgentEventSink {
    let id = id.to_string();

    let handler = move |ev: Value| {
        // `.output` 旁路：全量事件（含高频 message_update）逐行落盘
        if let Some(path) = output.as_deref() {
            output_file::append_event(path, &ev);
        }

        let kind = ev.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match kind {
            "turn_end" => with_state(|st| {
                if let Some(r) = st.records.get_mut(&id) {
                    r.turns += 1;
                    r.activity = None;
                }
            }),
            "message_end" => {
                let usage_val = with_state(|st| {
                    let r = st.records.get_mut(&id)?;
                    let m = ev.get("message")?;
                    r.usage.add_from_message(m);
                    r.transcript.push(m.clone());
                    m.get("usage").cloned()
                });

                if let Some(v) = usage_val
                    && config().report_usage
                    && let Ok(u) = serde_json::from_value::<Usage>(v)
                {
                    pool_usage(u);
                }
            }
            // widget 的"当前活动"：工具开始置位、结束清空。
            // 嵌套调用（`ctx.execute_tool`）不参与：活动名与工具计数只算顶层调用，
            // 否则一次编排会把 widget 的活动闪烁好几次、计数也偏大。
            "tool_execution_start" if !is_nested_call_event(&ev) => with_state(|st| {
                if let Some(r) = st.records.get_mut(&id) {
                    r.activity = ev
                        .get("toolName")
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                }
            }),
            "tool_execution_end" if !is_nested_call_event(&ev) => with_state(|st| {
                if let Some(r) = st.records.get_mut(&id) {
                    r.tool_uses += 1;
                    r.activity = None;
                }
            }),
            // 子会话压缩：计数并广播 `subagents:compacted`
            "compaction_end" => {
                let snap = with_state(|st| {
                    let r = st.records.get_mut(&id)?;
                    r.compaction_count += 1;
                    Some(r.clone())
                });

                if let Some(rec) = snap {
                    let reason = ev.get("reason").and_then(|v| v.as_str()).unwrap_or("");
                    let tokens_before =
                        ev.get("tokensBefore").and_then(|v| v.as_u64()).unwrap_or(0);
                    events::compacted(&rec, reason, tokens_before);
                }
            }
            _ => {}
        }
    };

    Arc::new(Mutex::new(Box::new(handler) as Box<dyn FnMut(Value) + Send>))
}

/// 谁在调用编排工具：直接读执行上下文里的身份。
///
/// `agent_id`/`depth` 是**扩展自己在 `SubAgentSpec` 里声明**的（它分配 id、它算深度），
/// 核心只是原样带进 child 的工具上下文再交回来。所以这里不需要反查记录，也不依赖任何指针相等。
#[derive(Debug, Clone, Default)]
pub(crate) struct Caller {
    /// None = 主会话
    pub agent_id: Option<String>,
    /// 主会话 0、它的子代理 1、孙代理 2…
    pub depth: u32,
}

/// 认出发起这次工具调用的 agent（上下文没带身份 = 主会话）。
pub(crate) fn caller_of(ctx: &ToolExecCtx) -> Caller {
    Caller {
        agent_id: ctx.agent_id.clone(),
        depth: ctx.depth,
    }
}

/// 按属主作用域找记录：子代理只能看见**自己**派发的那些。
pub(crate) fn owned_record(caller: &Caller, key: &str) -> Result<AgentRecord, String> {
    let rec = record(key).ok_or_else(|| format!("unknown agent id: {key}"))?;
    match caller.agent_id.as_deref() {
        None => Ok(rec),
        Some(owner) if rec.parent_agent_id.as_deref() == Some(owner) => Ok(rec),
        Some(_) => Err(format!(
            "agent {} was not spawned by this agent — nested delegation can only see its own children",
            rec.id
        )),
    }
}

/// 收尾时掐掉自己派发的子代理（父代理结束，子孙就没有归属了）。
fn abort_owned_children(parent_id: &str) {
    let owned: Vec<String> = with_state(|st| {
        st.records
            .values()
            .filter(|r| r.parent_agent_id.as_deref() == Some(parent_id) && !r.status.is_terminal())
            .map(|r| r.id.clone())
            .collect()
    });

    for id in owned {
        _ = stop(&id);
    }
}

/// 分发一次子代理（前台阻塞至结束、后台立即返回）。
///
/// 只有**请求本身**不合法（未知 resume 目标、目标仍在运行）才返回 `Err`；
/// 物化或执行失败会写进记录（`status = error`）并正常返回，由工具层格式化。
pub async fn dispatch(
    ctx: &ToolExecCtx,
    ty: &AgentType,
    req: SpawnRequest,
) -> Result<Dispatched, String> {
    dispatch_with_id(ctx, ty, req, None).await
}

/// 同 [`dispatch`]，但允许在**记录 id 一确定**时回调一次。
///
/// 工作流用它把 `recordId` 立刻写进进度条目：这样 agent 还在跑（或还在排队）时，
/// 检查器就能 `c` 打开它的会话——否则 `recordId` 只在结算时才补，运行中的行没有。
/// 回调在记录入库之后、开始 await 之前触发，因此回调里 `manager::record(id)` 一定拿得到。
pub async fn dispatch_with_id(
    ctx: &ToolExecCtx,
    ty: &AgentType,
    mut req: SpawnRequest,
    on_id: Option<Arc<dyn Fn(String) + Send + Sync>>,
) -> Result<Dispatched, String> {
    let (id, resume_path) = resolve_resume(&req)?;
    resolve_and_scope_model(&mut req, ty)?;

    // `.output` 转写（两条 spawn 路径共用；未开启时为 None）
    let output = req
        .output_transcript
        .then(|| open_output_file(&ctx.cwd, &id))
        .flatten();

    // 实际生效型号：显式型号已解析成 canonical；继承则用父级 ctx 透出的型号。
    let effective_model = req.model.clone().or_else(|| ctx.parent_model.clone());

    let background = req.run_in_background;
    let (done_tx, done_rx) = tokio::sync::watch::channel(false);

    let new_record = AgentRecord {
        id: id.clone(),
        name: req.name.clone(),
        handle: None,
        alias: None,
        agent_type: ty.name.clone(),
        display_name: ty.label().to_string(),
        description: req.description.clone(),
        status: AgentStatus::Queued,
        background,
        color: ty.color.clone(),
        activity: None,
        turns: 0,
        tool_uses: 0,
        usage: UsageTotals::default(),
        started_ms: now_ms(),
        ended_ms: None,
        result: None,
        transcript: Vec::new(),
        session_path: resume_path.clone(),
        output_path: output.as_ref().map(|p| p.display().to_string()),
        max_turns: req.max_turns,
        steer: None,
        abort: None,
        stop_requested: false,
        consumed: false,
        worktree: None,
        worktree_result: None,
        parent_agent_id: req.parent_agent_id.clone(),
        depth: req.depth,
        workflow_owned: req.workflow_owned,
        model: effective_model,
        compaction_count: 0,
        done_rx: Some(done_rx),
    };

    // 记录会出现在 dock widget 里（按 `widget` 配置可见）→ 自动打开停靠面板，
    // 对齐 plan-mode/goal 的"有活儿就自动展示"（前台代理同样受益：面板随即显示进度）。
    let dock_visible = widget::visible(&new_record, config().widget);
    let queued = insert_run_record(&id, new_record, &req, ty, background, &done_tx);

    if dock_visible {
        request_show_dock();
    }

    // 记录已入库：立刻把 id 回传给调用方（工作流据此在运行中就登记 `recordId`）。
    if let Some(cb) = on_id.as_ref() {
        cb(id.clone());
    }

    // 顶层生命周期：created（嵌套/工作流派发不发）
    if let Some(rec) = self::record(&id) {
        events::created(&rec);
    }

    if background {
        if !queued {
            start_background(ctx, &id).await;
        }

        return Ok(Dispatched {
            id,
            background: true,
            queued,
        });
    }

    // 前台没有后台启动钩子：在这里标运行，并发送 started（后台由 `start_background` 发）。
    mark_running(&id);

    if let Some(rec) = self::record(&id) {
        events::started(&rec);
    }

    // 前台：等前台额度 → 物化 → 跑 → 收尾（不发通知）。
    // 放到独立任务里跑再 await：主回合被硬中止（Esc）时，drop 的只是这个 await，
    // 真正的 run 任务照旧存活，靠 `parent_abort` 联动中止并走到 `finish`——
    // 否则前台 run 随 prompt future 一起消失，`finish` 永不执行，记录永远停在 running
    // （dock 里一直计时，看起来像“子代理停不下来”）。
    let fg_ctx = ctx.clone();
    let fg_ty = ty.clone();
    let fg_req = req.clone();
    let fg_id = id.clone();
    let handle = tokio::spawn(async move {
        run_foreground(&fg_ctx, &fg_ty, &fg_req, &fg_id, resume_path, output).await;
    });
    _ = handle.await;

    Ok(Dispatched {
        id,
        background: false,
        queued: false,
    })
}

/// resume：解析目标、校验状态，并沿用其 id 与 session。
fn resolve_resume(req: &SpawnRequest) -> Result<(String, Option<String>), String> {
    match req.resume.as_deref() {
        None => Ok((next_id(), None)),
        Some(key) => {
            let (id, path) = with_state(|st| {
                let Some(rec) = find_record(st, key) else {
                    return Err(format!("unknown agent id: {key}"));
                };

                if !rec.status.is_terminal() {
                    return Err(format!(
                        "agent {} is still {}; use steer_subagent instead of resume",
                        rec.id,
                        rec.status.as_str()
                    ));
                }

                Ok((rec.id.clone(), rec.session_path.clone()))
            })?;

            let path =
                path.ok_or_else(|| format!("agent {id} has no persisted session to resume"))?;

            Ok((id, Some(path)))
        }
    }
}

/// 型号：先解析成 canonical `provider/id`（fuzzy，核心只接受精确形式），
/// 再按 `scope_models` 检查是否在 `enabledModels` 之内。
///
/// 解析失败是**请求本身**的问题（与未知 resume 目标同类）→ 直接 `Err`。
fn resolve_and_scope_model(req: &mut SpawnRequest, ty: &AgentType) -> Result<(), String> {
    if let Some(input) = req.model.as_deref() {
        let canonical = model::resolve(input)?;
        req.model = Some(canonical);
    }

    match model::check_scope(
        config().scope_models,
        req.model.as_deref(),
        req.model_source,
        ty.label(),
        req.model.as_deref(),
    ) {
        ScopeVerdict::Ok => {}
        ScopeVerdict::Refuse(message) => return Err(message),
        ScopeVerdict::Warn(message) => {
            let key = format!("scope|{}|{message}", ty.name);
            if note_warning(&key) {
                util_notify(&message, UiNotifyLevel::Warning);
            }
        }
    }
    Ok(())
}

/// 插入新记录（或重置 resume 的既有记录）并决定本次是否需要排队；返回是否已排队。
fn insert_run_record(
    id: &str,
    record: AgentRecord,
    req: &SpawnRequest,
    ty: &AgentType,
    background: bool,
    done_tx: &Sender<bool>,
) -> bool {
    with_state(|st| {
        match st.records.get_mut(id) {
            // resume：复用既有记录，重置本次运行的状态（保留转写历史）
            Some(existing) => {
                existing.name = record.name.clone();
                existing.description = record.description.clone();
                // 先回「等启动」态，而不是直接 Running：后台起点 `start_background` 只接管 Queued 的记录，
                // 前台起点 `mark_running` 也会自行置 Running。
                // 这里若直接写 Running，后台 resume 会被 `start_background` 静默跳过
                // （记录永远停在 Running、runner 从未被调 → 主页无任何输出）。
                existing.status = AgentStatus::Queued;
                existing.background = background;
                existing.color = record.color.clone();
                existing.activity = None;
                existing.turns = 0;
                existing.tool_uses = 0;
                existing.usage = UsageTotals::default();
                existing.started_ms = now_ms();
                existing.ended_ms = None;
                existing.result = None;
                existing.max_turns = record.max_turns;
                existing.workflow_owned = record.workflow_owned;
                existing.model = record.model.clone();
                existing.compaction_count = 0;
                existing.steer = None;
                existing.abort = None;
                existing.stop_requested = false;
                existing.consumed = false;
                existing.done_rx = record.done_rx.clone();
            }
            None => {
                // 新记录：分配 `@handle` / `@alias`（共用命名空间，避让所有存活记录）
                let (handle, alias) = allocate_handles(st, &ty.name, req.name.as_deref());

                let mut record = record;
                record.handle = Some(handle);
                record.alias = alias;

                st.order.push(id.to_string());
                st.records.insert(id.to_string(), record);
            }
        }
        st.pending.insert(
            id.to_string(),
            PendingSpawn {
                req: req.clone(),
                ty: ty.clone(),
                done_tx: Some(done_tx.clone()),
                pending_steer: Vec::new(),
            },
        );

        prune_finished(st);

        // `bypass_queue`（定时任务）：不排队也不占额度——配额是给人的突发行为用的
        let should_queue =
            background && !req.bypass_queue && st.running_bg >= st.cfg.max_concurrent;

        if should_queue {
            st.queue.push_back(id.to_string());
            if let Some(r) = st.records.get_mut(id) {
                r.status = AgentStatus::Queued;
            }
        }

        should_queue
    })
}

/// 前台执行尾巴：等额度 → 建 worktree → 物化 → 跑 → 关副本 → 记账（不发通知）。
async fn run_foreground(
    ctx: &ToolExecCtx,
    ty: &AgentType,
    req: &SpawnRequest,
    id: &str,
    resume_path: Option<String>,
    output: Option<PathBuf>,
) {
    acquire_foreground_slot(&ctx.parent_abort).await;

    let base_cwd = ctx.cwd.clone();
    let work_cwd = match prepare_cwd(req, &base_cwd, id).await {
        Ok(cwd) => cwd,
        Err(e) => {
            finish(id, Err(e), false);
            return;
        }
    };

    let mut runner =
        match build_child_runner(ctx, ty, req, id, &work_cwd, &base_cwd, output, resume_path) {
            Ok(r) => r,
            Err(e) => {
                finish(id, Err(e), false);
                return;
            }
        };

    let controls = runner.controls();
    register_controls(id, &controls);

    let outcome = run_linked(&mut runner, req.prompt.clone(), ctx.parent_abort.clone()).await;
    let outcome = close_worktree(&base_cwd, id, outcome).await;
    finish(id, outcome, false);
}

/// 组装并物化 child runner（前台/后台共用）。
///
/// worktree 的建立与失败收尾由调用方负责：前台失败只记账，后台还要 `close_worktree` 并推下一个排队项。
#[allow(clippy::too_many_arguments)]
fn build_child_runner(
    ctx: &ToolExecCtx,
    ty: &AgentType,
    req: &SpawnRequest,
    id: &str,
    work_cwd: &str,
    base_cwd: &str,
    output: Option<PathBuf>,
    resume_path: Option<String>,
) -> Result<Box<dyn SubAgentRunner>, String> {
    let cfg = config();
    let mut spec = build_spec(
        ty,
        req,
        work_cwd,
        Some(make_sink(id, output)),
        resume_path,
        cfg.max_subagent_depth,
        cfg.default_max_turns,
        cfg.grace_turns,
    );

    apply_work_cwd(&mut spec, base_cwd, work_cwd);
    set_child_identity(&mut spec, id, req.depth);
    materialize(ctx, spec)
}

/// 把记录标为运行中（前台路径用；后台在 `start_background` 里设）。
fn mark_running(id: &str) {
    with_state(|st| {
        if let Some(r) = st.records.get_mut(id) {
            r.status = AgentStatus::Running;
            r.started_ms = now_ms();
        }
    });
}

/// 把 child 的身份写进 spec（id 在 `dispatch` 里已经分配好，深度来自请求）。
fn set_child_identity(spec: &mut SubAgentSpec, id: &str, depth: u32) {
    spec.agent_id = Some(id.to_string());
    spec.depth = depth;
}

/// worktree 隔离：按需建副本并把工作目录指过去；建不出来就**严格失败**（不静默用主目录）。
///
/// 失败即 `Err(可读原因)`：调用方要的是"在隔离环境里跑"，悄悄改成在主目录跑等于骗它。
async fn prepare_cwd(req: &SpawnRequest, cwd: &str, id: &str) -> Result<String, String> {
    if req.isolation.as_deref() != Some("worktree") {
        return Ok(cwd.to_string());
    }

    if !config().worktree_isolation {
        // 用户总开关关掉：**丢弃**请求而不是报错（关掉的是能力，不是调用者的错）
        return Ok(cwd.to_string());
    }

    let wt = worktree::create(cwd, id).await.map_err(|e| {
        format!(
            "Cannot run with isolation: \"worktree\" — {e}. \
                 Initialize git and commit at least once, or omit `isolation`."
        )
    })?;
    let work = wt.work_path.clone();

    with_state(|st| {
        if let Some(r) = st.records.get_mut(id) {
            r.worktree = Some(wt);
        }
    });

    Ok(work)
}

/// 把 spec 的工作目录指到副本（隔离时）。`build_spec` 的 `cwd` 参数只用于提示词/技能发现，
/// 子代理真正的工作目录是 spec 的 `cwd`（None = 继承父级）。
fn apply_work_cwd(spec: &mut SubAgentSpec, base_cwd: &str, work_cwd: &str) {
    if work_cwd != base_cwd {
        spec.cwd = Some(work_cwd.to_string());
    }
}

/// 跑完之后收拾副本：有改动就提交到分支，并把这件事写进结果正文。
async fn close_worktree(
    base_cwd: &str,
    id: &str,
    outcome: Result<String, String>,
) -> Result<String, String> {
    let found: Option<(Worktree, String)> = with_state(|st| {
        st.records
            .get(id)
            .and_then(|r| r.worktree.clone().map(|w| (w, r.description.clone())))
    });

    let Some((mut wt, description)) = found else {
        return outcome;
    };

    let result = worktree::cleanup(base_cwd, &mut wt, &description).await;
    let note = match (result.has_changes, result.branch.as_deref()) {
        (true, Some(branch)) => format!(
            "\n\n[isolation: worktree — changes committed to branch {branch} in the main checkout]"
        ),
        _ => String::new(),
    };

    with_state(|st| {
        if let Some(r) = st.records.get_mut(id) {
            r.worktree_result = Some(result.clone());
        }
    });

    outcome.map(|text| format!("{text}{note}"))
}

/// 启动一个排队中的后台子代理；物化失败时按"立即失败"收尾并顺延队首。
async fn start_background(ctx: &ToolExecCtx, id: &str) {
    let prepared = with_state(|st| {
        if st.records.get(id).map(|r| r.status) != Some(AgentStatus::Queued) {
            return None;
        }

        let p = st.pending.get(id)?;
        let req = p.req.clone();
        let ty = p.ty.clone();
        let pending_steer = p.pending_steer.clone();

        st.running_bg += 1;

        if let Some(r) = st.records.get_mut(id) {
            r.status = AgentStatus::Running;
            r.started_ms = now_ms();
        }

        Some((req, ty, pending_steer))
    });

    let Some((req, ty, pending_steer)) = prepared else {
        return;
    };

    if let Some(rec) = record(id) {
        events::started(&rec);
    }

    let resume_path = if req.resume.is_some() {
        with_state(|st| st.records.get(id).and_then(|r| r.session_path.clone()))
    } else {
        None
    };

    // `.output`：排队分支在**真正启动**时才建文件（排队时长不计入转写）
    let output = req
        .output_transcript
        .then(|| open_output_file(&ctx.cwd, id))
        .flatten();

    if let Some(display) = output.as_ref().map(|p| p.display().to_string()) {
        with_state(|st| {
            if let Some(r) = st.records.get_mut(id) {
                r.output_path = Some(display);
            }
        });
    }

    // worktree 隔离：副本在**跑之前**建好（失败就别留下半跑的 agent）。
    // 这段是 awaited 的 git 调用，所以 `start_background` 本身成了 async。
    // 但**物化仍在这里同步完成**，于是"物化失败"照旧在返回前就写进记录（不把失败推后成一个任务跳）。
    let base_cwd = ctx.cwd.clone();
    let work_cwd = match prepare_cwd(&req, &base_cwd, id).await {
        Ok(cwd) => cwd,
        Err(e) => {
            finish(id, Err(e), true);
            start_next_queued(ctx);
            return;
        }
    };

    let mut runner = match build_child_runner(
        ctx,
        &ty,
        &req,
        id,
        &work_cwd,
        &base_cwd,
        output,
        resume_path,
    ) {
        Ok(r) => r,
        Err(e) => {
            let e = close_worktree(&base_cwd, id, Err(e)).await.unwrap_err();
            finish(id, Err(e), true);
            start_next_queued(ctx);
            return;
        }
    };

    let controls = runner.controls();
    register_controls(id, &controls);

    let id_owned = id.to_string();
    let ctx_owned = ctx.clone();
    let prompt = req.prompt.clone();
    let parent_abort = ctx.parent_abort.clone();
    tokio::spawn(async move {
        // 与父级取消信号联动：主回合被硬中止（Esc）时，后台子代理不能变成无人收尾的孤儿。
        let outcome = run_linked(&mut runner, prompt, parent_abort).await;
        let outcome = close_worktree(&base_cwd, &id_owned, outcome).await;
        complete_background(&ctx_owned, &id_owned, outcome);
    });

    // 排队期间积累的 steer
    for msg in pending_steer {
        controls.steer_text(msg);
    }
}

/// 启动队首（若有）。
///
/// 建副本（worktree 隔离）是异步的，而调用点（收尾路径）都是同步的 →
/// 这里把"启动一个排队项"整段丢进一个任务；调用方不等待。
fn start_next_queued(ctx: &ToolExecCtx) {
    let ctx_owned = ctx.clone();
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };

    handle.spawn(async move { start_next_queued_inner(&ctx_owned).await });
}

/// 启动队首：跳过已被清理/中止的记录，取第一个仍处 `Queued` 的项后台拉起。
async fn start_next_queued_inner(ctx: &ToolExecCtx) {
    let next = with_state(|st| {
        while let Some(id) = st.queue.pop_front() {
            match st.records.get(&id) {
                Some(r) if r.status == AgentStatus::Queued => return Some(id),
                _ => continue, // 记录已被清理/中止
            }
        }
        None
    });

    if let Some(id) = next {
        start_background(ctx, &id).await;
    }
}

/// 占用一个前台额度（`foreground_max_concurrent == 0` 时不限）。
///
/// 满额时按 [`PARENT_ABORT_POLL`] 轮询等待；父级回合已中止时不再等
/// （直接跑，由 [`run_linked`] 立即联动中止，避免中止路径被额度卡住）。
async fn acquire_foreground_slot(parent_abort: &Arc<AtomicBool>) {
    loop {
        let acquired = with_state(|st| {
            let limit = st.cfg.foreground_max_concurrent;
            if limit == 0 || st.running_fg < limit {
                st.running_fg += 1;
                true
            } else {
                false
            }
        });

        if acquired || parent_abort.load(Ordering::Relaxed) {
            return;
        }

        tokio::time::sleep(PARENT_ABORT_POLL).await;
    }
}

/// 前台运行：与父级取消信号 select，父级中止则联动中止子代理。
async fn run_linked(
    runner: &mut Box<dyn SubAgentRunner>,
    prompt: String,
    parent_abort: Arc<AtomicBool>,
) -> Result<String, String> {
    let controls = runner.controls();
    let run = runner.run(prompt);
    let watch = wait_for_parent_abort(parent_abort);

    futures_util::pin_mut!(run, watch);

    match select(run, watch).await {
        Either::Left((res, _)) => res,
        Either::Right((_, run)) => {
            controls.request_abort();
            // 协作式取消走的是 Ok 路径（child 吐一条 aborted 消息后就正常返回），
            // 但这里的结局是「父级中止」：如实报错，否则记录会被 `finish` 记成 completed。
            let _ = run.await;
            Err("Operation aborted".to_string())
        }
    }
}

/// 轮询等待父级中止标志置位（间隔 [`PARENT_ABORT_POLL`]）。
async fn wait_for_parent_abort(flag: Arc<AtomicBool>) {
    while !flag.load(Ordering::Relaxed) {
        tokio::time::sleep(PARENT_ABORT_POLL).await;
    }
}

/// 把运行期句柄写进记录。
fn register_controls(id: &str, controls: &SubAgentControls) {
    with_state(|st| {
        if let Some(r) = st.records.get_mut(id) {
            r.steer = Some(controls.steer.clone());
            r.abort = Some(controls.abort.clone());
            if controls.session_path.is_some() {
                r.session_path = controls.session_path.clone();
            }
        }
    });
}

/// 收尾：写状态/结果/结束时间、唤醒等待者、释放后台额度。
///
/// 返回需要发完成通知的记录（后台且记录仍存在）。
fn finish(id: &str, outcome: Result<String, String>, background: bool) -> Option<AgentRecord> {
    // 父代理结束 → 它派发的子孙就没有归属了
    abort_owned_children(id);

    let (notify_rec, event_rec) = with_state(|st| {
        let failed = outcome.is_err();

        if let Some(r) = st.records.get_mut(id) {
            match &outcome {
                Ok(text) if !text.trim().is_empty() => r.result = Some(text.clone()),
                Ok(_) => {}
                Err(e) => r.result = Some(e.clone()),
            }

            r.status = r.infer_terminal_status(failed);
            if r.ended_ms.is_none() {
                r.ended_ms = Some(now_ms());
            }
        }

        // 嵌套子代理的 token 花费向上汇报给父代理。在同一个锁里做，避免父代理恰好这时收尾而错过累加。
        let rollup = st
            .records
            .get(id)
            .and_then(|r| r.parent_agent_id.clone().map(|pid| (pid, r.usage)));

        if let Some((pid, usage)) = rollup
            && let Some(parent) = st.records.get_mut(&pid)
        {
            parent.usage.merge(&usage);
        }

        if let Some(tx) = st.pending.get(id).and_then(|p| p.done_tx.as_ref()) {
            _ = tx.send(true);
        }

        if background {
            st.running_bg = st.running_bg.saturating_sub(1);
        } else {
            st.running_fg = st.running_fg.saturating_sub(1);
        }

        // 已读（`get_subagent_result` / RPC consume）的不再发完成通知
        let notify_rec = st
            .records
            .get(id)
            .cloned()
            .filter(|r| r.background && !r.consumed);
        let event_rec = st.records.get(id).cloned();

        prune_finished(st);

        (notify_rec, event_rec)
    });

    // 锁已释放再广播（handler 可能回调 manager 而再次取锁）
    if let Some(rec) = event_rec {
        events::settled(&rec);
    }

    // 这是最后一个收尾的代理（队列空、无运行中的工作流）→ 已完成记录不再占 dock，
    // 面板随即自动收起。有别的活儿在跑时 `dismiss_finished` 是 no-op。
    dismiss_finished();

    notify_rec
}

/// 淘汰超出上限的已终结记录（保留最新的）。
fn prune_finished(st: &mut State) {
    let finished: Vec<String> = st
        .order
        .iter()
        .filter(|id| st.records.get(*id).is_some_and(|r| r.status.is_terminal()))
        .cloned()
        .collect();

    if finished.len() <= MAX_FINISHED_RECORDS {
        return;
    }

    let drop_count = finished.len() - MAX_FINISHED_RECORDS;
    for id in finished.into_iter().take(drop_count) {
        st.records.remove(&id);
        st.pending.remove(&id);
        st.order.retain(|o| o != &id);
    }
}

/// 按 id / name / `@handle` / `@alias` 查找记录（name 冲突时取最新的）。
///
/// 允许 `@` 前缀（对齐输入框 `@` 候选）：`/agents stop @explore-2` 与 `stop explore-2` 等价。
fn find_record<'a>(st: &'a State, key: &str) -> Option<&'a AgentRecord> {
    let key = key.trim();
    let key = key.strip_prefix('@').unwrap_or(key);
    if let Some(r) = st.records.get(key) {
        return Some(r);
    }

    st.order
        .iter()
        .rev()
        .filter_map(|id| st.records.get(id))
        .find(|r| {
            r.name.as_deref() == Some(key)
                || r.handle
                    .as_deref()
                    .is_some_and(|h| h.eq_ignore_ascii_case(key))
                || r.alias
                    .as_deref()
                    .is_some_and(|a| a.eq_ignore_ascii_case(key))
        })
}

/// 存活记录已占用的句柄/别名集合（淘汰即回池：只扫当前记录）。
fn taken_handles(st: &State) -> HashSet<String> {
    let mut set = HashSet::new();

    for r in st.records.values() {
        if let Some(h) = &r.handle {
            set.insert(h.clone());
        }

        if let Some(a) = &r.alias {
            set.insert(a.clone());
        }
    }

    set
}

/// 为新记录分配类型派生 `handle` 与（可选的）name 派生 `alias`。
/// 两者共用命名空间：alias 避让 `taken` ∪ 刚分配的 handle。
fn allocate_handles(st: &State, type_name: &str, name: Option<&str>) -> (String, Option<String>) {
    let taken = taken_handles(st);
    let handle = mention::assign_handle(&mention::handle_base(type_name), &taken);

    let alias = name.map(|n| {
        let mut scope = taken.clone();
        scope.insert(handle.clone());
        mention::assign_handle(&mention::handle_base(n), &scope)
    });

    (handle, alias)
}

/// 按 `@handle` 解析存活记录（匹配 handle 或 alias，大小写不敏感）。
/// 不做 tombstone：淘汰即不可达（会话切换本就会清空全部）。
pub fn resolve_mention(handle: &str) -> Option<AgentRecord> {
    with_state(|st| {
        st.order
            .iter()
            .filter_map(|id| st.records.get(id))
            .find(|r| {
                r.handle
                    .as_deref()
                    .is_some_and(|h| h.eq_ignore_ascii_case(handle))
                    || r.alias
                        .as_deref()
                        .is_some_and(|a| a.eq_ignore_ascii_case(handle))
            })
            .cloned()
    })
}

/// 记录快照（供工具层与 `/agents` 展示）。
pub fn record(key: &str) -> Option<AgentRecord> {
    with_state(|st| find_record(st, key).cloned())
}

/// 等待一个代理终结，返回其记录（记录被清理时返回 None）。
pub async fn wait(key: &str) -> Option<AgentRecord> {
    let mut rx = with_state(|st| find_record(st, key).and_then(|r| r.done_rx.clone()))?;
    if !*rx.borrow() {
        _ = rx.changed().await;
    }
    record(key)
}

/// 把一个已终结的代理标为"结果已读"，抑制它的完成通知。
///
/// 记录不存在、或还没终结时返回 `false`。已标读/已投递的，再标一次也无害（返回 `true`）。
pub fn consume_result(key: &str) -> bool {
    with_state(|st| match find_record(st, key) {
        Some(r) if r.status.is_terminal() => {
            let id = r.id.clone();
            if let Some(r) = st.records.get_mut(&id) {
                r.consumed = true;
            }
            true
        }
        _ => false,
    })
}

/// 向运行中的代理注入 steer；排队中的代理暂存，启动时注入。
pub fn steer(key: &str, message: &str) -> Result<(), String> {
    if message.trim().is_empty() {
        return Err("steer message must not be empty".to_string());
    }

    let id = with_state(|st| -> Result<String, String> {
        let Some(rec) = find_record(st, key) else {
            return Err(format!("unknown agent id: {key}"));
        };

        let id = rec.id.clone();
        let status = rec.status;
        let steer = rec.steer.clone();

        match status {
            AgentStatus::Running => match steer {
                Some(f) => {
                    f(message.to_string());
                    Ok(id)
                }
                // 刚被标记 running 但句柄还没登记：极短窗口
                None => Err(format!("agent {id} is starting; retry in a moment")),
            },
            AgentStatus::Queued => {
                if let Some(p) = st.pending.get_mut(&id) {
                    p.pending_steer.push(message.to_string());
                }
                Ok(id)
            }
            other => Err(format!(
                "agent {id} is {} and cannot be steered",
                other.as_str()
            )),
        }
    })?;

    events::steered(&id, message); // 锁已释放再广播

    Ok(())
}

/// 主动停止一个代理（运行中置位 abort；排队中直接出队并终结）。
pub fn stop(key: &str) -> Result<(), String> {
    let (id, notify) = with_state(|st| {
        let Some(rec) = find_record(st, key) else {
            return Err(format!("unknown agent id: {key}"));
        };

        let id = rec.id.clone();
        let status = rec.status;
        if status.is_terminal() {
            return Err(format!("agent {id} is already {}", status.as_str()));
        }

        let abort = rec.abort.clone();
        let background = rec.background;
        if let Some(r) = st.records.get_mut(&id) {
            r.stop_requested = true;
        }

        let mut notify = false;
        match abort {
            Some(a) => a.store(true, Ordering::Relaxed),
            // 排队中（或尚未登记句柄）：出队并立即终结
            None => {
                st.queue.retain(|q| q != &id);

                if let Some(r) = st.records.get_mut(&id) {
                    r.status = AgentStatus::Stopped;
                    r.ended_ms = Some(now_ms());
                }

                if let Some(tx) = st.pending.get(&id).and_then(|p| p.done_tx.as_ref()) {
                    _ = tx.send(true);
                }

                notify = background;
            }
        }

        Ok((id, notify))
    })?;

    if notify {
        notify_by_id(&id);
    }

    Ok(())
}

/// 硬中止（Esc/Ctrl+C）：停掉所有**顶层**未终结记录（运行中 + 排队）。
///
/// 顶层的子代理（`Agent` 工具前台、`@mention`/tasks 的后台）挂在本轮 run 上，
/// 主回合被丢弃后没人再管它们（dock 会一直显示 running）；工作流派发的子代理
/// （`workflow_owned`）走自己的取消标志，不在此列——它们该继续跑。
pub fn abort_top_level() {
    let ids: Vec<String> = with_state(|st| {
        st.order
            .iter()
            .filter_map(|id| st.records.get(id))
            .filter(|r| events::is_top_level(r) && !r.status.is_terminal())
            .map(|r| r.id.clone())
            .collect()
    });

    for id in ids {
        // 停失败只可能是记录已终结（并发），忽略。
        _ = stop(&id);
    }
}

/// 中止全部代理并清空注册表（会话切换 / 扩展被禁用）。
///
/// 已 spawn 的任务仍会走到各自的 `finish`，但记录已不在表中，因此不会再发通知、也不会重复计数。
pub fn reset_all() {
    with_state(|st| {
        for id in st.order.clone() {
            let abort = st.records.get(&id).and_then(|r| r.abort.clone());
            if let Some(r) = st.records.get_mut(&id) {
                r.stop_requested = true;
            }
            if let Some(a) = abort {
                a.store(true, Ordering::Relaxed);
            }
            if let Some(tx) = st.pending.get(&id).and_then(|p| p.done_tx.as_ref()) {
                let _ = tx.send(true);
            }
        }

        st.records.clear();
        st.pending.clear();
        st.order.clear();
        st.queue.clear();
        st.running_bg = 0;
        st.running_fg = 0;
        st.warned.clear();
        st.batch.clear();
        st.batch_generation = st.batch_generation.wrapping_add(1);
        st.dismissed_before_ms = 0;
    });

    reset_usage_pool();
}

/// 待合并通知的一条。
///
/// 批次只需要知道"有一条通知以及它的正文"，**不需要知道它是什么**：于是
/// [`super::workflow::task`] 也能复用同一套 join 窗口/代际而不倒挂依赖。
pub(crate) enum BatchItem {
    /// 一个已完成后台代理（信封在投递时从记录渲染）。
    Agent(String),
    /// 已经渲染好的：continuation 信封正文 + 可选卡片载荷（`custom_type`, `data`）。
    /// `tokens`/`duration_ms` 是该条的用量（合并通知外层 `Σ` 合计用）。
    Rendered {
        /// 已渲染好的 continuation 信封正文，合并时直接拼接。
        envelope: String,
        /// 可选卡片载荷 `(custom_type, data)`；无卡片时为 None。
        card: Option<(String, Value)>,
        /// 该条通知的 token 用量，外层合并通知按 Σ 汇总。
        tokens: u64,
        /// 该条通知的耗时（毫秒），外层合并通知按 Σ 汇总。
        duration_ms: u64,
    },
}

/// 正在运行的后台代理数（工作流侧算"还有同伴在跑"用）。
pub(crate) fn background_live() -> usize {
    with_state(|st| st.running_bg)
}

/// 把一条通知攒进 join 批次（窗口/代际逻辑与后台代理共用）。
///
/// `peers_live`：此刻**除它自己以外**还有没有在跑的后台工作（代理或 run）。
/// 有则等窗口到期兜底，没有则立刻合并投递——这正是"同轮发 3 个，只出一条消息"。
pub(crate) fn batch_push(item: BatchItem, peers_live: bool) {
    let (mode, generation) = with_state(|st| {
        st.batch.push(item);
        (st.cfg.join, st.batch_generation)
    });

    if mode == JoinMode::Async || !peers_live {
        flush_batch();
        return;
    }

    let window = mode.window_ms();
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async move {
            tokio::time::sleep(Duration::from_millis(window)).await;
            let stale = with_state(|st| st.batch_generation != generation);
            if !stale {
                flush_batch();
            }
        });
    } else {
        flush_batch();
    }
}

/// 记录列表：运行中（含排队）在前，其后按结束时间倒序。
pub fn list() -> Vec<AgentRecord> {
    with_state(|st| {
        let mut recs: Vec<AgentRecord> = st
            .order
            .iter()
            .filter_map(|id| st.records.get(id).cloned())
            .collect();

        recs.sort_by_key(|r| {
            (
                if r.status.is_terminal() { 1 } else { 0 },
                std::cmp::Reverse(r.ended_ms.unwrap_or(r.started_ms)),
            )
        });
        recs
    })
}

/// widget 行（无可见代理时返回空 = dock 不占位）。
///
/// 每帧由 `dock_sections()` 调用：只取一次状态锁并克隆记录，不读盘。
/// 已终结且低于隐藏水位（用户已开新回合）的记录不进 widget：它们已完成、
/// 通知卡片也已落到消息流，继续占 dock 只会白占状态栏上方的位置。
pub fn widget_lines() -> Vec<DockLine> {
    let (records, queued, cfg) = with_state(|st| {
        let dismissed_before = st.dismissed_before_ms;
        let mut recs: Vec<AgentRecord> = st
            .order
            .iter()
            .filter_map(|id| st.records.get(id).cloned())
            .filter(|r| !r.status.is_terminal() || r.ended_ms.is_some_and(|e| e > dismissed_before))
            .collect();

        recs.sort_by_key(|r| {
            (
                if r.status.is_terminal() { 1 } else { 0 },
                std::cmp::Reverse(r.ended_ms.unwrap_or(r.started_ms)),
            )
        });

        (recs, st.queue.len(), st.cfg.clone())
    });

    widget::lines(&records, queued, &cfg)
}

/// 用户提交了新的 prompt、或全部代理/工作流都已收尾：把此刻**已完成**的子代理记录从 dock 隐去。
///
/// 只在没有任何在跑的活儿（后台/前台代理、排队、工作流 run）时生效：
/// 有活儿在跑说明用户还在等结果，dock 应该继续占位（且终端记录与运行中记录
/// 混排时贸然隐去会让面板闪烁）。运行中/排队的记录不受影响；之后新完成的记录
/// （`ended_ms` 大于水位）照常显示。
///
/// 两个触发点：用户开新回合（`Subagent::on_user_submit`），以及最后一个
/// 代理/工作流运行收尾（[`finish`] 与 `workflow::tool::settle_run`）。后者对齐 `tasks`
/// 扩展「列表空了 dock 自然收起」的手感：活儿一停就收，不必等下一句输入。
///
/// 记录并不删除：`/agents` 面板、`@handle` resume、用量统计读到的还是同一份。
pub(crate) fn dismiss_finished() {
    let busy = with_state(|st| st.running_bg > 0 || st.running_fg > 0 || !st.queue.is_empty());
    if busy || workflow::task::has_live() {
        return;
    }

    let now = now_ms();
    with_state(|st| st.dismissed_before_ms = now);
}

/// 告警去重：同一键每会话只返回一次 true。
pub fn note_warning(key: &str) -> bool {
    with_state(|st| st.warned.insert(key.to_string()))
}

/// 发送后台完成通知（`Continuation` 携带完整结果 + 人向卡片 `CustomMessage`）。
///
/// 卡片与 `Continuation` 的分工：
/// 卡片给人看（状态/用量/结果预览 + session 路径，只留预览，所以不与长文重复）；
/// `Continuation` 给模型看（完整结果，且随会话持久化）。
fn notify_by_id(id: &str) {
    let rec = with_state(|st| st.records.get(id).cloned());

    if let Some(rec) = rec {
        request_ui(ExtensionUiRequest::Continuation {
            message: Box::new(notify::continuation_message(&rec)),
        });

        push_card(&rec);
    }
}

/// 投递一张完成卡片（实时显示）并落盘（`/resume` 后由 `on_session_switched` 重放）。
fn deliver_card(custom_type: &str, payload: Value) {
    request_ui(ExtensionUiRequest::CustomMessage {
        label: EXT.to_string(),
        custom_type: custom_type.to_string(),
        data: payload.clone(),
    });

    request_ui(ExtensionUiRequest::PersistSessionEntry {
        custom_type: custom_type.to_string(),
        data: payload,
    });
}

/// 投递一条 agent 完成卡片（从记录生成载荷）。
fn push_card(rec: &AgentRecord) {
    deliver_card(CARD_TYPE, notify::card_payload(rec));
}

/// 后台任务完成后：按 join 策略投递通知 + 顺延队首。
///
/// `smart`/`group`：完成时若仍有后台在跑，先攒进批次；等最后一个完成（或窗口到期）时合并成一条通知。
/// 这样"同轮发 3 个后台代理"只产生一条对话消息，而不是三条。
fn complete_background(ctx: &ToolExecCtx, id: &str, outcome: Result<String, String>) {
    if finish(id, outcome, true).is_none() {
        start_next_queued(ctx);
        return;
    }

    let peers_live = with_state(|st| st.running_bg > 0);
    batch_push(BatchItem::Agent(id.to_string()), peers_live);
    start_next_queued(ctx);
}

/// 投递当前批次：1 条 → 普通通知；多条 → 合并通知（一条给模型 + 逐个卡片）。
///
/// 递增代际让旧定时器失效。新增一种通知（如工作流运行完成）只需在推入端
/// 自己渲染好 envelope/card，这里完全不必认识它。
fn flush_batch() {
    let items = with_state(|st| {
        let items = std::mem::take(&mut st.batch);
        st.batch_generation = st.batch_generation.wrapping_add(1);
        items
    });

    if items.is_empty() {
        return;
    }

    let mut total_tokens: u64 = 0;
    let mut total_duration: u64 = 0;
    let mut envelopes: Vec<String> = Vec::new();
    let mut cards: Vec<(String, Value)> = Vec::new();

    for item in items {
        match item {
            BatchItem::Agent(id) => {
                let rec = with_state(|st| st.records.get(&id).cloned());
                if let Some(rec) = rec
                    && !rec.consumed
                {
                    total_tokens = total_tokens.saturating_add(rec.usage.display_total());
                    total_duration = total_duration.saturating_add(rec.duration_ms());
                    envelopes.push(notify::result_envelope(&rec));
                    cards.push((CARD_TYPE.to_string(), notify::card_payload(&rec)));
                }
            }
            BatchItem::Rendered {
                envelope,
                card,
                tokens,
                duration_ms,
            } => {
                total_tokens = total_tokens.saturating_add(tokens);
                total_duration = total_duration.saturating_add(duration_ms);
                envelopes.push(envelope);

                if let Some(c) = card {
                    cards.push(c);
                }
            }
        }
    }

    if envelopes.is_empty() {
        return;
    }

    let text = if envelopes.len() == 1 {
        envelopes.remove(0)
    } else {
        notify::batch_envelope(&envelopes, total_tokens, total_duration)
    };

    request_ui(ExtensionUiRequest::Continuation {
        message: Box::new(AgentMessage::user_text(&text)),
    });

    for (custom_type, data) in cards {
        deliver_card(&custom_type, data);
    }
}

/// 测试：钉住配置缓存（只有钉住它的那个线程能改；见 `State::cfg_pinned`）。
#[cfg(test)]
pub(crate) fn pin_config_for_test() {
    let me = std::thread::current().id();
    with_state(|st| {
        st.cfg_pinned = true;
        st.cfg_pin_owner = Some(me);
    });
}

/// 测试：放开（解锁时调用）。
#[cfg(test)]
pub(crate) fn unpin_config_for_test() {
    with_state(|st| {
        st.cfg_pinned = false;
        st.cfg_pin_owner = None;
    });
}

/// 测试互斥锁：本扩展的进程级注册表与 UI 队列是全局态，
/// 同 crate 内所有触碰它们的测试都必须串行。
#[cfg(test)]
pub(crate) fn test_lock() -> TestLock {
    /// 跨测试串行锁（所有触碰全局态的测试共用）。
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    let guard = L
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // 持锁期间**钉住配置**：别的线程（上个用例遗留的后台任务）调 `config()`/`reload_config()`
    // 时不要重载——它们的 agent_dir 是各自线程的 override，键必然"变化"，一重载就把当前
    // 用例刚设的配置清成缺省。钉住的线程自己不受影响（它可能刚写了配置文件）。
    pin_config_for_test();
    TestLock { _guard: guard }
}

/// 测试锁 + 配置钉住；解锁时自动放开（见 [`test_lock`]）。
#[cfg(test)]
pub(crate) struct TestLock {
    /// 持有测试锁直到 Drop（Drop 时同时放开配置钉住）。
    _guard: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
impl Drop for TestLock {
    /// 释放测试锁时同步解除配置钉住。
    fn drop(&mut self) {
        unpin_config_for_test();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::extensions::{MakeSubAgentFn, SubAgentSpec};
    use crate::extensions::subagent::types::AgentSource;

    /// 假 runner：不碰网络与 LLM，可被 abort，可被要求"停留"以观察运行中状态。
    struct FakeRunner {
        /// 可被 abort 的标志（测试里手动置位）。
        abort: Arc<AtomicBool>,
        /// steer 收集到的消息。
        inbox: Arc<Mutex<Vec<String>>>,
        /// 每次 run 前停留的毫秒数（观察运行中状态）。
        hold_ms: u64,
    }

    impl SubAgentRunner for FakeRunner {
        fn controls(&self) -> SubAgentControls {
            let inbox = self.inbox.clone();
            SubAgentControls {
                steer: Arc::new(move |t: String| inbox.lock().unwrap().push(t)),
                abort: self.abort.clone(),
                session_path: None,
            }
        }

        fn run(
            &mut self,
            prompt: String,
        ) -> futures_util::future::BoxFuture<'_, Result<String, String>> {
            Box::pin(async move {
                if self.hold_ms > 0 {
                    let steps = self.hold_ms / 5;
                    for _ in 0..steps.max(1) {
                        if self.abort.load(Ordering::Relaxed) {
                            return Err("Operation aborted".to_string());
                        }
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                }
                if self.abort.load(Ordering::Relaxed) {
                    return Err("Operation aborted".to_string());
                }
                Ok(format!("echo:{prompt}"))
            })
        }
    }

    /// 记录物化时收到的 spec，供断言 prompt 模式等。
    #[derive(Default, Clone)]
    struct SeenSpec {
        /// 物化时收到的 spec（断言 prompt 模式等）。
        inner: Arc<Mutex<Vec<SubAgentSpec>>>,
        /// 每次 run 前停留的毫秒数。
        hold_ms: u64,
    }

    fn fake_ctx(seen: SeenSpec, parent_abort: Arc<AtomicBool>) -> ToolExecCtx {
        let hold_ms = seen.hold_ms;
        let make = Arc::new(move |spec: SubAgentSpec| {
            seen.inner.lock().unwrap().push(spec.clone());
            Ok(Box::new(FakeRunner {
                abort: Arc::new(AtomicBool::new(false)),
                inbox: Arc::new(Mutex::new(Vec::new())),
                hold_ms,
            }) as Box<dyn SubAgentRunner>)
        }) as Arc<MakeSubAgentFn>;
        ToolExecCtx {
            execute_tool: crate::core::extensions::unavailable_tool_exec(),
            parent_tool_call_id: None,
            nested_calls: Default::default(),
            session_branch_entries: Default::default(),
            script_tools: Default::default(),
            cwd: ".".to_string(),
            make_sub_agent: make,
            parent_abort,
            agent_id: None,
            depth: 0,
            parent_model: None,
            script_call: false,
        }
    }

    fn ty(name: &str) -> AgentType {
        AgentType {
            name: name.to_string(),
            display_name: name.to_string(),
            description: "d".to_string(),
            color: None,
            tools: Some(vec!["read".to_string()]),
            model: None,
            thinking: None,
            max_turns: None,
            prompt_mode: PromptMode::Replace,
            enabled: true,
            persist_session: None,
            session_dir: None,
            system_prompt: "you are a specialist".to_string(),
            source: AgentSource::Builtin,
            source_path: None,
            disallowed_tools: None,
            // 默认测试类型开启嵌套（不限型），以保留既有断言；opt-in 阧门另有用例。
            allowed_subagents: Some(Vec::new()),
            memory: None,
            isolated: None,
            isolation: None,
            extensions: None,
            extensions_none: false,
            exclude_extensions: Vec::new(),
            skills: None,
            output_transcript: None,
            run_in_background: None,
            inherit_context: None,
            ignored_fields: Vec::new(),
        }
    }

    fn req(prompt: &str, background: bool) -> SpawnRequest {
        SpawnRequest {
            prompt: prompt.to_string(),
            description: "task".to_string(),
            name: None,
            tools: Some(vec!["read".to_string()]),
            model: None,
            model_source: crate::extensions::subagent::model::ModelSource::Inherited,
            bypass_queue: false,
            isolation: None,
            parent_agent_id: None,
            depth: 0,
            thinking: None,
            max_turns: None,
            run_in_background: background,
            inherit_context: false,
            disallowed_tools: None,
            isolated: false,
            extensions: None,
            exclude_extensions: Vec::new(),
            skills: Vec::new(),
            clear_skills: false,
            output_transcript: false,
            persist_session: false,
            session_dir: None,
            resume: None,
            injected_tools: Vec::new(),
            workflow_owned: false,
        }
    }

    /// 测试之间共享进程级注册表，必须串行；并把配置固定为缺省，
    /// 避免读到开发者本机 `~/.prux/extensions/subagent.json`。
    ///
    /// 同时把配置缓存**钉住**（[`set_cfg_pinned`]）：上个用例遗留的后台任务（定时扫描、
    /// 工作流收尾…）会调 `config()`，而它们的 agent_dir override 与本次不同 → 判定缓存失效
    /// → 重载 → 把本次刚设的配置清成缺省。钉住之后这些重载在用例内不再发生。
    fn lock() -> super::TestLock {
        let guard = super::test_lock();
        super::pin_config_for_test();
        let key = config::config_key();
        #[cfg(test)]
        eprintln!("DEBUG lock() acquire");
        with_state(|st| {
            st.cfg = SubagentConfig::default();
            st.cfg_key = key;
        });
        guard
    }

    /// 一行 widget 文本（断言用）。
    fn widget_text(line: &DockLine) -> String {
        line.iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn dispatch_allocates_handles_and_resolves_mentions() {
        let _g = lock();
        reset_all();
        rt().block_on(async {
            let ctx = fake_ctx(SeenSpec::default(), Arc::new(AtomicBool::new(false)));
            let d1 = dispatch(&ctx, &ty("Explore"), req("a", true))
                .await
                .unwrap();
            let r1 = record(&d1.id).unwrap();
            assert_eq!(r1.handle.as_deref(), Some("explore"));
            assert!(r1.alias.is_none());

            let d2 = dispatch(&ctx, &ty("Explore"), req("b", true))
                .await
                .unwrap();
            assert_eq!(record(&d2.id).unwrap().handle.as_deref(), Some("explore-2"));
            assert_eq!(resolve_mention("EXPLORE-2").unwrap().id, d2.id);

            // 具名实例：alias 从 name slug 化派生，且与 handle 共用命名空间
            let mut named = req("c", true);
            named.name = Some("Probe One".to_string());
            let d3 = dispatch(&ctx, &ty("Explore"), named).await.unwrap();
            let r3 = record(&d3.id).unwrap();
            assert_eq!(r3.handle.as_deref(), Some("explore-3"));
            assert_eq!(r3.alias.as_deref(), Some("probe-one"));
            assert_eq!(resolve_mention("probe-one").unwrap().id, d3.id);

            // resume 保留原 handle（不重新分配）
            with_state(|st| {
                if let Some(r) = st.records.get_mut(&d1.id) {
                    r.session_path = Some("/tmp/s.jsonl".to_string());
                }
            });
            let mut resume = req("more", true);
            resume.resume = Some(d1.id.clone());
            let dr = dispatch(&ctx, &ty("Explore"), resume).await.unwrap();
            assert_eq!(dr.id, d1.id);
            assert_eq!(record(&d1.id).unwrap().handle.as_deref(), Some("explore"));
        });
        reset_all();
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Runtime::new().unwrap()
    }

    /// 取走并计数队列里的 ShowDock 请求（其余请求一并丢弃）。
    fn drain_dock_requests() -> usize {
        let mut n = 0;
        while let Some(req) = crate::core::extensions::take_pending_ui() {
            if matches!(req, ExtensionUiRequest::ShowDock) {
                n += 1;
            }
        }
        n
    }

    /// 派发的新记录会出现在 dock widget 里 → 自动请求打开停靠面板（对齐 plan-mode/goal），
    /// 且同一时刻只排一条（展示意图幂等，不能把后续完成通知挤后）。
    /// `widget = off` 时它不会进 dock，就不该无端弹面板。
    #[test]
    fn dispatch_auto_opens_the_dock_for_visible_records() {
        // 队列与 plan-mode/goal 共用：断言精确条数时必须与它们串行。
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = lock();
        reset_all();
        drain_dock_requests();

        // 默认 widget = all：后台代理可见 → ShowDock；两次派发也只排一条
        let ctx = fake_ctx(SeenSpec::default(), Arc::new(AtomicBool::new(false)));
        rt().block_on(async {
            dispatch(&ctx, &ty("Explore"), req("go", true))
                .await
                .unwrap();
            dispatch(&ctx, &ty("Explore"), req("also", true))
                .await
                .unwrap();
        });
        assert_eq!(
            drain_dock_requests(),
            1,
            "可见代理应请求打开停靠面板（重复请求合并为一条）"
        );

        // widget = off：dock 里不会出现 → 不请求
        with_state(|st| st.cfg.widget = config::WidgetMode::Off);
        rt().block_on(async {
            dispatch(&ctx, &ty("Explore"), req("again", true))
                .await
                .unwrap();
        });
        assert_eq!(drain_dock_requests(), 0, "widget=off 不应请求打开面板");
        reset_all();
    }

    #[test]
    fn foreground_spec_uses_replace_prompt_and_eager_finish() {
        let _g = lock();
        reset_all();
        let seen = SeenSpec::default();
        let ctx = fake_ctx(seen.clone(), Arc::new(AtomicBool::new(false)));
        rt().block_on(async {
            let d = dispatch(&ctx, &ty("Explore"), req("go", false))
                .await
                .unwrap();
            assert!(!d.background);
            let rec = record(&d.id).unwrap();
            assert_eq!(rec.status, AgentStatus::Completed);
            assert_eq!(rec.result.as_deref(), Some("echo:go"));
            assert!(rec.ended_ms.is_some());
        });
        let specs = seen.inner.lock().unwrap();
        let spec = &specs[0];
        assert!(spec.clear_context_files);
        assert!(
            spec.system_prompt
                .as_deref()
                .unwrap()
                .starts_with("<active_agent name=\"Explore\"/>")
        );
        assert!(
            spec.system_prompt
                .as_deref()
                .unwrap()
                .contains("you are a specialist")
        );
        assert!(spec.append_system_prompt.is_none());
        assert!(spec.resume_session_path.is_none());
        drop(specs);
        reset_all();
    }

    #[test]
    fn memory_injects_block_and_tools() {
        let _g = lock();
        let _ad = crate::test_support::AgentDirGuard::temp();
        reset_all();
        let seen = SeenSpec::default();
        let ctx = fake_ctx(seen.clone(), Arc::new(AtomicBool::new(false)));
        let mut t = ty("Explore");
        t.memory = Some(crate::extensions::subagent::types::MemoryScope::User);
        rt().block_on(async {
            // 只读分支（req.tools = ["read"]，无 write/edit）
            dispatch(&ctx, &t, req("go", false)).await.unwrap();
        });
        {
            let specs = seen.inner.lock().unwrap();
            let ap = specs[0].append_system_prompt.as_deref().unwrap();
            assert!(ap.contains("Agent Memory"), "{ap}");
            assert!(ap.contains("read-only"), "{ap}");
        }
        seen.inner.lock().unwrap().clear();
        // 读写分支：有效工具含 write → 完整块 + 补 read/write/edit
        let mut rw = req("go", false);
        rw.tools = Some(vec!["read".into(), "write".into()]);
        rt().block_on(async {
            dispatch(&ctx, &t, rw).await.unwrap();
        });
        {
            let specs = seen.inner.lock().unwrap();
            let ap = specs[0].append_system_prompt.as_deref().unwrap();
            assert!(ap.contains("Memory Instructions"), "{ap}");
            let tools = specs[0].tools.as_ref().unwrap();
            for n in ["read", "write", "edit"] {
                assert!(tools.iter().any(|t| t == n), "{tools:?}");
            }
        }
        reset_all();
    }

    #[test]
    fn append_mode_wraps_instructions_and_bridge() {
        let mut t = ty("general-purpose");
        t.prompt_mode = PromptMode::Append;
        let spec = build_spec(&t, &req("go", false), "/tmp", None, None, 2, 0, 5);
        assert!(spec.system_prompt.is_none());
        assert!(!spec.clear_context_files);
        let ap = spec.append_system_prompt.unwrap();
        assert!(ap.contains("<sub_agent_context>"));
        assert!(ap.contains("<agent_instructions>"));
        assert!(ap.contains("<active_agent name=\"general-purpose\"/>"));
    }

    /// `scope_models` 开启时：**调用方**给的越界型号被拒绝（请求级 Err），
    /// frontmatter 钉的只告警（照常派发）。
    #[test]
    fn scope_models_refuses_caller_override_and_warns_for_frontmatter() {
        let _ad = crate::test_support::AgentDirGuard::temp();
        let _g = test_lock();
        let cwd = tempfile::tempdir().unwrap();
        let cwd = cwd.path().to_string_lossy().to_string();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut ty = ty("audit");

        // 开启 scope + 只允许 haiku，并钉住目录（测试环境里真目录是空的）
        let cfg = config::SubagentConfig {
            scope_models: true,
            ..Default::default()
        };
        config::write_config(&config::config_path(), &cfg);
        reload_config();
        crate::extensions::subagent::model::set_catalog_for_test(&[
            ("anthropic", "claude-haiku-4-5", "Claude Haiku 4.5"),
            ("anthropic", "claude-sonnet-4-5", "Claude Sonnet 4.5"),
        ]);
        crate::core::settings_manager::write_enabled_models(Some(&[
            "anthropic/claude-haiku-4-5".to_string()
        ]))
        .unwrap();

        let ctx = ToolExecCtx {
            execute_tool: crate::core::extensions::unavailable_tool_exec(),
            parent_tool_call_id: None,
            nested_calls: Default::default(),
            session_branch_entries: Default::default(),
            script_tools: Default::default(),
            cwd: cwd.clone(),
            make_sub_agent: std::sync::Arc::new(|_| Err("unused".to_string())),
            parent_abort: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            agent_id: None,
            depth: 0,
            parent_model: None,
            script_call: false,
        };

        // 调用方给的越界型号 → Err（不静默改成别的）
        let mut caller_req = req("x", false);
        caller_req.model = Some("anthropic/claude-sonnet-4-5".to_string());
        caller_req.model_source = crate::extensions::subagent::model::ModelSource::Caller;
        let err = rt.block_on(dispatch(&ctx, &ty, caller_req)).unwrap_err();
        assert!(err.contains("Model not in scope"), "{err}");

        // frontmatter 钉的越界型号 → 照常派发（只是告警）
        ty.model = Some("anthropic/claude-sonnet-4-5".to_string());
        let mut fm_req = req("x", false);
        fm_req.model = Some("anthropic/claude-sonnet-4-5".to_string());
        fm_req.model_source = crate::extensions::subagent::model::ModelSource::Frontmatter;
        let dispatched = rt
            .block_on(dispatch(&ctx, &ty, fm_req))
            .expect("frontmatter 越界不该拒绝");
        assert!(record(&dispatched.id).is_some());
        for _ in 0..100 {
            if record(&dispatched.id).is_some_and(|r| r.status.is_terminal()) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        // 无法解析的型号 → 请求级 Err（可读）
        let mut bad_req = req("x", false);
        bad_req.model = Some("no-such-model-xyz".to_string());
        bad_req.model_source = crate::extensions::subagent::model::ModelSource::Caller;
        let err = rt.block_on(dispatch(&ctx, &ty, bad_req)).unwrap_err();
        assert!(err.contains("Model not found"), "{err}");

        // 复位：关掉 scope、清 allowlist 与目录
        config::write_config(&config::config_path(), &config::SubagentConfig::default());
        reload_config();
        crate::core::settings_manager::write_enabled_models(None).unwrap();
        crate::extensions::subagent::model::set_catalog_for_test(&[]);
        reset_all();
    }

    /// worktree 隔离（端到端）：副本里跑、工作目录指过去、收尾把副本删掉；
    /// 不在仓库里**严格报错**；项目开关关掉时丢弃请求（不报错、在原目录跑）。
    #[test]
    fn worktree_isolation_runs_in_a_copy_and_fails_loudly_outside_a_repo() {
        let _ad = crate::test_support::AgentDirGuard::temp();
        let _g = test_lock();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_string_lossy().to_string();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&cwd)
                .output()
                .expect("git")
        };
        if !git(&["init", "-q"]).status.success() {
            return; // 没有 git：跳过（不假装通过）
        }
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "init"]);

        let wait_terminal = |id: &str| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                if let Some(r) = record(id)
                    && r.status.is_terminal()
                {
                    return r;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "agent 没在预期时间内结束"
                );
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        };

        // 1) 真仓库：隔离的后台 agent 在副本里跑
        // （让假 runner 停留一下：副本在跑完就会被收掉，断言要在它还活着的时候做）
        reset_all();
        let seen = SeenSpec {
            hold_ms: 300,
            ..Default::default()
        };
        let mut ctx = fake_ctx(seen.clone(), Arc::new(AtomicBool::new(false)));
        ctx.cwd = cwd.clone(); // 假 ctx 默认用进程 cwd；这里要跑在临时仓库里
        let mut isolated = req("x", true);
        isolated.isolation = Some("worktree".to_string());
        let dispatched = rt
            .block_on(dispatch(&ctx, &ty("general-purpose"), isolated))
            .expect("dispatch");
        let spec_cwd = seen.inner.lock().unwrap()[0]
            .cwd
            .clone()
            .expect("隔离 spawn 的 spec 应带 cwd");
        assert!(spec_cwd.contains("prux-agent-"), "工作在副本里: {spec_cwd}");
        assert!(
            std::path::Path::new(&spec_cwd).join("a.txt").exists(),
            "副本应是仓库的拷贝: {spec_cwd}"
        );
        // agent "改了文件"（副本还活着）→ 收尾要提交到分支，并把分支名写进结果
        std::fs::write(std::path::Path::new(&spec_cwd).join("a.txt"), "two\n").unwrap();
        let rec = wait_terminal(&dispatched.id);
        assert_eq!(rec.status, AgentStatus::Completed, "{:?}", rec.result);
        let wt_result = rec.worktree_result.clone().expect("worktree 收尾结果");
        assert!(wt_result.has_changes, "有改动应如实报");
        let branch = wt_result.branch.clone().expect("分支名");
        assert!(branch.starts_with("prux-agent-"), "{branch}");
        assert!(
            rec.result
                .as_deref()
                .is_some_and(|r| r.contains(&branch) && r.contains("worktree")),
            "结果正文要点名分支: {:?}",
            rec.result
        );
        assert!(!std::path::Path::new(&spec_cwd).exists(), "副本收尾后删掉");

        // 2) 不在仓库里：严格报错（记录里是失败，不是静默用主目录）
        let outside = tempfile::tempdir().unwrap();
        let outside_ctx = fake_ctx(SeenSpec::default(), Arc::new(AtomicBool::new(false)));
        let mut ctx_outside = outside_ctx.clone();
        ctx_outside.cwd = outside.path().to_string_lossy().to_string();
        reset_all();
        let mut isolated = req("x", true);
        isolated.isolation = Some("worktree".to_string());
        let dispatched = rt
            .block_on(dispatch(&ctx_outside, &ty("general-purpose"), isolated))
            .expect("dispatch 本身成功（失败落在记录里）");
        let rec = wait_terminal(&dispatched.id);
        assert_eq!(rec.status, AgentStatus::Error);
        assert!(
            rec.result
                .as_deref()
                .is_some_and(|r| r.contains("Cannot run with isolation")),
            "{:?}",
            rec.result
        );

        // 3) 项目开关关掉：丢弃请求，仍在原目录跑（不报错）
        let cfg = config::SubagentConfig {
            worktree_isolation: false,
            ..Default::default()
        };
        config::write_config(&config::config_path(), &cfg);
        reload_config();
        reset_all();
        let seen2 = SeenSpec::default();
        let mut ctx2 = fake_ctx(seen2.clone(), Arc::new(AtomicBool::new(false)));
        ctx2.cwd = cwd.clone();
        let mut isolated = req("x", true);
        isolated.isolation = Some("worktree".to_string());
        let dispatched = rt
            .block_on(dispatch(&ctx2, &ty("general-purpose"), isolated))
            .expect("dispatch");
        let spec_cwd = seen2.inner.lock().unwrap()[0].cwd.clone();
        assert!(
            spec_cwd.is_none(),
            "关掉后不建副本：spec.cwd 留空 = 继承父级目录（也就是原目录）"
        );
        wait_terminal(&dispatched.id);

        config::write_config(&config::config_path(), &config::SubagentConfig::default());
        reload_config();
        reset_all();
    }

    #[test]
    fn tools_and_model_are_forwarded_to_spec() {
        let t = ty("E");
        let mut r = req("go", false);
        r.tools = Some(vec!["read".to_string(), "grep".to_string()]);
        r.model = Some("anthropic/claude-haiku-4-5".to_string());
        r.thinking = Some("high".to_string());
        r.max_turns = Some(7);
        let spec = build_spec(
            &t,
            &r,
            "/tmp",
            None,
            Some("/tmp/s.jsonl".to_string()),
            2,
            0,
            5,
        );
        // 请求里的工具原样保留，外加**嵌套委派**放行的三个编排工具（核心的递归防护只放行
        // 显式列在这里的名字；见 `nested`）
        let tools = spec.tools.as_ref().unwrap();
        assert_eq!(&tools[..2], &["read".to_string(), "grep".to_string()]);
        for name in crate::extensions::subagent::nested::NESTED_TOOL_NAMES {
            assert!(tools.iter().any(|t| t == name), "缺 {name}: {tools:?}");
        }
        assert_eq!(spec.model.as_deref(), Some("anthropic/claude-haiku-4-5"));
        assert_eq!(spec.thinking.as_deref(), Some("high"));
        assert_eq!(spec.max_turns, Some(7));
        assert_eq!(spec.resume_session_path.as_deref(), Some("/tmp/s.jsonl"));

        // 到顶的 child 不再放行编排工具
        let mut deep = req("go", false);
        deep.tools = Some(vec!["read".to_string()]);
        deep.depth = 2;
        let spec = build_spec(&t, &deep, "/tmp", None, None, 2, 0, 5);
        assert_eq!(
            spec.tools.as_ref().unwrap(),
            &vec!["read".to_string()],
            "到顶就不再写进白名单"
        );

        // 上限调到 1（= 关掉嵌套）：深度 1 的 child（主会话的子代理）不再放行编排工具
        // 上限 1（= 关掉嵌套）：深度 1 的 child（主会话的子代理）不再放行编排工具
        let mut child = req("go", false);
        child.tools = Some(vec!["read".to_string()]);
        child.depth = 1;
        let spec = build_spec(&t, &child, "/tmp", None, None, 1, 0, 5);
        assert_eq!(
            spec.tools.as_ref().unwrap(),
            &vec!["read".to_string()],
            "上限 1 = 子代理不能再派（depth 1 是主会话的子代理）"
        );
        // 上限 2：同一个请求放行
        let spec = build_spec(&t, &child, "/tmp", None, None, 2, 0, 5);
        assert!(
            spec.tools.as_ref().unwrap().iter().any(|t| t == "Agent"),
            "上限 2 时 depth 1 还能再派一代"
        );
    }

    /// `default_max_turns` 只在调用方/frontmatter 都没给时生效；`grace_turns` 原样进 spec。
    #[test]
    fn build_spec_applies_default_max_turns_and_grace_turns() {
        let t = ty("E");
        let spec = build_spec(&t, &req("go", false), "/tmp", None, None, 2, 12, 3);
        assert_eq!(spec.max_turns, Some(12), "调用方未给时取配置缺省");
        assert_eq!(spec.grace_turns, Some(3));
        // 显式给了 max_turns → 调用方赢
        let mut r = req("go", false);
        r.max_turns = Some(2);
        let spec = build_spec(&t, &r, "/tmp", None, None, 2, 12, 3);
        assert_eq!(spec.max_turns, Some(2));
        // 缺省 0 = 不限
        let spec = build_spec(&t, &req("go", false), "/tmp", None, None, 2, 0, 5);
        assert_eq!(spec.max_turns, None);
    }

    /// 嵌套是 **opt-in**：agent 文件没写 `allowed_subagents` 就拿不到委派工具；
    /// 写了（`all` 或列表）且未到深度上限才给。
    #[test]
    fn nested_tools_are_opt_in_via_allowed_subagents() {
        let mut t = ty("Boss");
        t.allowed_subagents = None;
        let spec = build_spec(&t, &req("go", false), "/tmp", None, None, 2, 0, 5);
        assert!(
            !spec.tools.as_ref().unwrap().iter().any(|x| x == "Agent"),
            "没写 allowed_subagents 不应放行委派工具: {:?}",
            spec.tools
        );
        t.allowed_subagents = Some(vec!["Explore".to_string()]);
        let spec = build_spec(&t, &req("go", false), "/tmp", None, None, 2, 0, 5);
        assert!(spec.tools.as_ref().unwrap().iter().any(|x| x == "Agent"));
        // `isolated` 下不给（上游 `!options.isolated`）
        let mut r = req("go", false);
        r.isolated = true;
        let spec = build_spec(&t, &r, "/tmp", None, None, 2, 0, 5);
        assert!(
            !spec.tools.as_ref().unwrap().iter().any(|x| x == "Agent"),
            "isolated 不应放行委派工具: {:?}",
            spec.tools
        );
    }

    /// 嵌套：从执行上下文认出调用者身份（主会话 / 某个子代理），以及属主作用域的查找。
    #[test]
    fn caller_identity_and_ownership_scope() {
        let _g = test_lock();
        reset_all();
        let parent_abort = Arc::new(AtomicBool::new(false));
        let child_abort = Arc::new(AtomicBool::new(false));
        // 一条 depth=1 的记录（parent = 主会话），以及它的取消标志
        let mut rec = crate::extensions::subagent::types::AgentRecord {
            id: "child1".to_string(),
            name: None,
            handle: None,
            alias: None,
            agent_type: "general-purpose".to_string(),
            display_name: "g".to_string(),
            description: "d".to_string(),
            status: AgentStatus::Running,
            background: false,
            color: None,
            activity: None,
            turns: 0,
            tool_uses: 0,
            usage: Default::default(),
            started_ms: 0,
            ended_ms: None,
            result: None,
            transcript: Vec::new(),
            session_path: None,
            output_path: None,
            max_turns: None,
            steer: None,
            abort: Some(child_abort.clone()),
            stop_requested: false,
            worktree: None,
            worktree_result: None,
            parent_agent_id: None,
            depth: 1,
            workflow_owned: false,
            model: None,
            compaction_count: 0,
            consumed: false,
            done_rx: None,
        };
        rec.id = "child1".to_string();
        with_state(|st| {
            st.records.insert("child1".to_string(), rec);
        });

        let ctx_for = |abort: &Arc<AtomicBool>, id: Option<&str>, depth: u32| ToolExecCtx {
            execute_tool: crate::core::extensions::unavailable_tool_exec(),
            parent_tool_call_id: None,
            nested_calls: Default::default(),
            session_branch_entries: Default::default(),
            script_tools: Default::default(),
            cwd: ".".to_string(),
            make_sub_agent: Arc::new(|_| Err("unused".to_string())),
            parent_abort: abort.clone(),
            agent_id: id.map(str::to_string),
            depth,
            parent_model: None,
            script_call: false,
        };
        // 主会话：上下文没带身份 → depth 0、无属主
        let main = caller_of(&ctx_for(&parent_abort, None, 0));
        assert!(main.agent_id.is_none());
        assert_eq!(main.depth, 0);
        // 子代理：身份随 spec/ctx 传下来
        let child = caller_of(&ctx_for(&child_abort, Some("child1"), 1));
        assert_eq!(child.agent_id.as_deref(), Some("child1"));
        assert_eq!(child.depth, 1);

        // 属主作用域：主会话看得到全部；子代理只看得到自己派发的
        assert!(owned_record(&main, "child1").is_ok(), "主会话不受限");
        assert!(owned_record(&child, "child1").is_err(), "不能看自己");
        let mut grandchild = crate::extensions::subagent::types::AgentRecord {
            id: "g1".to_string(),
            name: None,
            handle: None,
            alias: None,
            agent_type: "general-purpose".to_string(),
            display_name: "g".to_string(),
            description: "d".to_string(),
            status: AgentStatus::Completed,
            background: false,
            color: None,
            activity: None,
            turns: 0,
            tool_uses: 0,
            usage: Default::default(),
            started_ms: 0,
            ended_ms: None,
            result: None,
            transcript: Vec::new(),
            session_path: None,
            output_path: None,
            max_turns: None,
            steer: None,
            abort: None,
            stop_requested: false,
            worktree: None,
            worktree_result: None,
            parent_agent_id: Some("child1".to_string()),
            depth: 2,
            workflow_owned: false,
            model: None,
            compaction_count: 0,
            consumed: false,
            done_rx: None,
        };
        grandchild.parent_agent_id = Some("child1".to_string());
        with_state(|st| {
            st.records.insert("g1".to_string(), grandchild);
        });
        assert!(owned_record(&child, "g1").is_ok(), "自己的子代理可见");
        match owned_record(&main, "nope") {
            Err(e) => assert!(e.contains("unknown agent id"), "{e}"),
            Ok(_) => panic!("未知 id 不该查到记录"),
        }
        reset_all();
    }

    /// 嵌套子代理的 token 花费在它收尾时累加到父代理。
    #[test]
    fn nested_child_usage_rolls_up_to_parent() {
        let _g = lock();
        reset_all();
        let make = |id: &str, parent: Option<&str>, depth: u32, usage: UsageTotals| AgentRecord {
            id: id.to_string(),
            name: None,
            handle: None,
            alias: None,
            agent_type: "general-purpose".to_string(),
            display_name: id.to_string(),
            description: "d".to_string(),
            status: AgentStatus::Running,
            background: false,
            color: None,
            activity: None,
            turns: 0,
            tool_uses: 0,
            usage,
            started_ms: 0,
            ended_ms: None,
            result: None,
            transcript: Vec::new(),
            session_path: None,
            output_path: None,
            max_turns: None,
            steer: None,
            abort: None,
            stop_requested: false,
            worktree: None,
            worktree_result: None,
            parent_agent_id: parent.map(str::to_string),
            depth,
            workflow_owned: false,
            model: None,
            compaction_count: 0,
            consumed: false,
            done_rx: None,
        };
        with_state(|st| {
            st.records.insert(
                "p1".to_string(),
                make("p1", None, 0, UsageTotals::default()),
            );
            st.records.insert(
                "c1".to_string(),
                make(
                    "c1",
                    Some("p1"),
                    1,
                    UsageTotals {
                        input: 100,
                        output: 50,
                        cache_read: 0,
                        cache_write: 10,
                        cost: 0.01,
                    },
                ),
            );
        });
        finish("c1", Ok("done".into()), false);
        let parent = record("p1").expect("parent");
        assert_eq!(parent.usage.input, 100);
        assert_eq!(parent.usage.output, 50);
        assert_eq!(parent.usage.cache_write, 10);
        assert!((parent.usage.cost - 0.01).abs() < 1e-9);
        reset_all();
    }

    #[test]
    fn foreground_follows_parent_abort() {
        let _g = lock();
        reset_all();
        let seen = SeenSpec {
            hold_ms: 1_000,
            ..Default::default()
        };
        let parent_abort = Arc::new(AtomicBool::new(false));
        let ctx = fake_ctx(seen, parent_abort.clone());
        let flag = parent_abort.clone();
        rt().block_on(async move {
            let handle =
                tokio::spawn(
                    async move { dispatch(&ctx, &ty("E"), req("go", false)).await.unwrap() },
                );
            // 等它进入运行，再模拟父级 Esc
            tokio::time::sleep(Duration::from_millis(60)).await;
            flag.store(true, Ordering::Relaxed);
            let d = handle.await.unwrap();
            let rec = record(&d.id).unwrap();
            assert!(rec.status.is_terminal(), "status={}", rec.status.as_str());
            assert_eq!(rec.result.as_deref(), Some("Operation aborted"));
        });
        reset_all();
    }

    #[test]
    fn background_follows_parent_abort() {
        // 后台子代理自成一个任务：不跟父级取消信号联动的话，它会在主回合被 Esc 丢弃后
        // 继续跑，dock 一直显示 running（tasks 已收尾，子代理却停不下来）。
        let _g = lock();
        reset_all();
        let seen = SeenSpec {
            hold_ms: 1_000,
            ..Default::default()
        };
        let parent_abort = Arc::new(AtomicBool::new(false));
        let ctx = fake_ctx(seen, parent_abort.clone());
        let flag = parent_abort.clone();
        rt().block_on(async move {
            let d = dispatch(&ctx, &ty("E"), req("go", true)).await.unwrap();
            assert!(d.background);
            tokio::time::sleep(Duration::from_millis(60)).await;
            assert_eq!(record(&d.id).unwrap().status, AgentStatus::Running);

            flag.store(true, Ordering::Relaxed);
            for _ in 0..200 {
                if record(&d.id)
                    .map(|r| r.status.is_terminal())
                    .unwrap_or(true)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let rec = record(&d.id).unwrap();
            assert!(rec.status.is_terminal(), "status={}", rec.status.as_str());
            assert_eq!(rec.result.as_deref(), Some("Operation aborted"));
        });
        reset_all();
    }

    /// 硬中止时只停顶层记录：嵌套子代理由父级 `finish` 连带收尾，
    /// 工作流派发的记录走自己的取消标志，不该被主会话的 Esc 波及。
    #[test]
    fn abort_top_level_spares_nested_and_workflow_owned_records() {
        let _g = lock();
        reset_all();
        let seen = SeenSpec {
            hold_ms: 1_000,
            ..Default::default()
        };
        let ctx = fake_ctx(seen, Arc::new(AtomicBool::new(false)));
        rt().block_on(async {
            let d = dispatch(&ctx, &ty("E"), req("go", true)).await.unwrap();
            tokio::time::sleep(Duration::from_millis(60)).await;
            assert_eq!(record(&d.id).unwrap().status, AgentStatus::Running);

            let make = |id: &str, parent: Option<&str>, workflow_owned: bool| AgentRecord {
                id: id.to_string(),
                name: None,
                handle: None,
                alias: None,
                agent_type: "E".to_string(),
                display_name: id.to_string(),
                description: "d".to_string(),
                status: AgentStatus::Running,
                background: true,
                color: None,
                activity: None,
                turns: 0,
                tool_uses: 0,
                usage: UsageTotals::default(),
                started_ms: 0,
                ended_ms: None,
                result: None,
                transcript: Vec::new(),
                session_path: None,
                output_path: None,
                max_turns: None,
                steer: None,
                abort: None,
                stop_requested: false,
                worktree: None,
                worktree_result: None,
                parent_agent_id: parent.map(str::to_string),
                depth: 1,
                workflow_owned,
                model: None,
                compaction_count: 0,
                consumed: false,
                done_rx: None,
            };
            with_state(|st| {
                for rec in [make("nested", Some("p"), false), make("wf", None, true)] {
                    st.order.push(rec.id.clone());
                    st.records.insert(rec.id.clone(), rec);
                }
            });

            abort_top_level();

            for _ in 0..200 {
                if record(&d.id)
                    .map(|r| r.status.is_terminal())
                    .unwrap_or(true)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert!(record(&d.id).unwrap().status.is_terminal());
            assert_eq!(record("nested").unwrap().status, AgentStatus::Running);
            assert_eq!(record("wf").unwrap().status, AgentStatus::Running);
        });
        reset_all();
    }

    /// 前台 run 在 dispatch future 被 drop（主回合硬中止）后必须自收尾：
    /// 否则 `finish` 永不执行，记录停在 running，dock 一直计时。
    #[test]
    fn foreground_run_survives_dropped_dispatch_and_finalizes() {
        let _g = lock();
        reset_all();
        let seen = SeenSpec {
            hold_ms: 1_000,
            ..Default::default()
        };
        let parent_abort = Arc::new(AtomicBool::new(false));
        let ctx = fake_ctx(seen, parent_abort.clone());
        let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let cap = captured.clone();
        rt().block_on(async move {
            let outer = tokio::spawn(async move {
                let cb = Arc::new(move |id: String| {
                    *cap.lock().unwrap() = Some(id);
                });
                dispatch_with_id(&ctx, &ty("E"), req("go", false), Some(cb)).await
            });

            for _ in 0..200 {
                if captured.lock().unwrap().is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            let id = captured.lock().unwrap().clone().expect("记录应已分配 id");

            // 模拟主回合被硬中止：drop dispatch future，再置位父级取消信号
            outer.abort();
            let _ = outer.await;
            parent_abort.store(true, Ordering::Relaxed);

            for _ in 0..200 {
                if record(&id).map(|r| r.status.is_terminal()).unwrap_or(true) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let rec = record(&id).expect("记录不应消失");
            assert!(
                rec.status.is_terminal(),
                "前台 run 必须自收尾，不能停在 running: {}",
                rec.status.as_str()
            );
        });
        reset_all();
    }

    #[test]
    fn background_queues_when_pool_is_full_and_can_be_steered_and_stopped() {
        let _g = lock();
        reset_all();
        with_state(|st| st.running_bg = st.cfg.max_concurrent);
        let seen = SeenSpec::default();
        let ctx = fake_ctx(seen, Arc::new(AtomicBool::new(false)));
        rt().block_on(async {
            let d = dispatch(&ctx, &ty("E"), req("t", true)).await.unwrap();
            assert!(d.queued, "pool is full → must queue");
            assert_eq!(record(&d.id).unwrap().status, AgentStatus::Queued);
            assert!(
                widget_lines()
                    .first()
                    .is_some_and(|l| widget_text(l).contains("1 queued")),
                "标题行应显示排队数"
            );
            steer(&d.id, "hurry").unwrap();
            with_state(|st| {
                assert_eq!(st.pending.get(&d.id).unwrap().pending_steer, vec!["hurry"]);
            });
            stop(&d.id).unwrap();
            assert_eq!(record(&d.id).unwrap().status, AgentStatus::Stopped);
            // 记录仍留在 widget（最近完成），但排队计数已归零
            let head = widget_lines().first().cloned().unwrap_or_default();
            assert!(
                !widget_text(&head).contains("queued"),
                "queued agent left the pool: {}",
                widget_text(&head)
            );
        });
        reset_all();
    }

    #[test]
    fn wait_resolves_for_queued_agent_that_is_stopped() {
        let _g = lock();
        reset_all();
        with_state(|st| st.running_bg = st.cfg.max_concurrent);
        let seen = SeenSpec::default();
        let ctx = fake_ctx(seen, Arc::new(AtomicBool::new(false)));
        rt().block_on(async {
            let d = dispatch(&ctx, &ty("E"), req("t", true)).await.unwrap();
            assert!(d.queued);
            let id = d.id.clone();
            let waiter = tokio::spawn(async move { wait(&id).await });
            tokio::time::sleep(Duration::from_millis(20)).await;
            stop(&d.id).unwrap();
            let rec = waiter.await.unwrap().unwrap();
            assert_eq!(rec.status, AgentStatus::Stopped);
        });
        reset_all();
    }

    #[test]
    fn background_completion_releases_slot_and_notifies() {
        // pending UI 队列是进程级全局态：与持 `AUTH_TEST_LOCK` 的 plan_mode 等测试串行，
        // 否则并发测试会把本用例的完成通知从队列里取走（通知为空）。
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = lock();
        reset_all();
        while crate::core::extensions::take_pending_ui().is_some() {}
        let seen = SeenSpec::default();
        let ctx = fake_ctx(seen, Arc::new(AtomicBool::new(false)));
        let mut kinds: Vec<&'static str> = Vec::new();
        let mut card_data = None;
        rt().block_on(async {
            let d = dispatch(&ctx, &ty("E"), req("hello", true)).await.unwrap();
            assert!(!d.queued);
            // 假 runner 立即返回；等完成信号
            let rec = wait(&d.id).await.unwrap();
            assert_eq!(rec.status, AgentStatus::Completed);
            assert_eq!(rec.result.as_deref(), Some("echo:hello"));

            // 完成通知：一条 Continuation（给模型，含全文）+ 一张卡片（给人，只留预览）+ 卡片落盘。
            // 通知在 `complete_background` 里于 `wait` 解消**之后**入队，故短轮询等它落队；
            // UI 队列是全局的，并发测试可能混入其它扩展请求，按 custom_type 容忍（不当成失败）。
            for _ in 0..50 {
                while let Some(req) = crate::core::extensions::take_pending_ui() {
                    match req {
                        ExtensionUiRequest::Continuation { message } => {
                            let text = message.text();
                            if text.contains("<subagent_result") {
                                assert!(text.contains("echo:hello"), "{text}");
                                kinds.push("continuation");
                            }
                        }
                        ExtensionUiRequest::CustomMessage {
                            custom_type, data, ..
                        } if custom_type == notify::CARD_TYPE => {
                            assert_eq!(data["status"], "completed");
                            assert!(data["preview"].as_str().unwrap().contains("echo:hello"));
                            card_data = Some(data);
                            kinds.push("card");
                        }
                        ExtensionUiRequest::PersistSessionEntry { custom_type, data }
                            if custom_type == notify::CARD_TYPE =>
                        {
                            assert_eq!(Some(data), card_data.clone(), "落盘载荷与卡片一致");
                            kinds.push("persist");
                        }
                        _ => {}
                    }
                }
                if kinds.len() >= 3 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        kinds.sort_unstable();
        assert_eq!(kinds, vec!["card", "continuation", "persist"]);
        reset_all();
    }

    /// join `smart`：同轮两个后台代理只产生**一条**合并通知。
    /// 中性批次：一个"已渲染"的条目（工作流运行完成）与代理通知共用同一套窗口与合并，
    /// 而 manager 完全不必认识它是什么。
    #[test]
    fn rendered_batch_items_merge_with_agent_notifications() {
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = lock();
        reset_all();
        while crate::core::extensions::take_pending_ui().is_some() {}
        with_state(|st| {
            st.cfg = SubagentConfig {
                join: JoinMode::Smart,
                ..Default::default()
            }
        });

        // 没有同伴在跑 → 立刻投递，且单条不带外层信封
        batch_push(
            BatchItem::Rendered {
                envelope: "<workflow_result id=\"wf_x\">ok</workflow_result>".to_string(),
                card: Some((
                    "subagent-workflow".to_string(),
                    serde_json::json!({ "name": "demo" }),
                )),
                tokens: 120,
                duration_ms: 500,
            },
            false,
        );
        let mut contracts: Vec<String> = Vec::new();
        let mut cards: Vec<String> = Vec::new();
        while let Some(r) = crate::core::extensions::take_pending_ui() {
            match r {
                ExtensionUiRequest::Continuation { message } => contracts.push(message.text()),
                ExtensionUiRequest::CustomMessage { custom_type, .. } => cards.push(custom_type),
                _ => {}
            }
        }
        assert_eq!(contracts.len(), 1, "{contracts:?}");
        assert!(contracts[0].contains("workflow_result"), "{}", contracts[0]);
        assert_eq!(cards, vec!["subagent-workflow".to_string()]);

        // 与另一个条目混在一起 → 攒批（"还有同伴在跑"需要在 runtime 里才能起窗口定时器），
        // 到期/显式 flush 时合并成一条，两者都在里面
        let mut merged: Vec<String> = Vec::new();
        rt().block_on(async {
            batch_push(
                BatchItem::Rendered {
                    envelope: "<workflow_result id=\"wf_y\">ok</workflow_result>".to_string(),
                    card: None,
                    tokens: 30,
                    duration_ms: 200,
                },
                true,
            );
            batch_push(
                BatchItem::Rendered {
                    envelope: "<subagent_result id=\"ab\">hi</subagent_result>".to_string(),
                    card: None,
                    tokens: 10,
                    duration_ms: 100,
                },
                true,
            );
            // 窗口是 30s，这里直接手动触发投递（窗口兜底路径由既有 join 用例覆盖）
            flush_batch();
            while let Some(r) = crate::core::extensions::take_pending_ui() {
                if let ExtensionUiRequest::Continuation { message } = r {
                    merged.push(message.text());
                }
            }
        });
        assert_eq!(merged.len(), 1, "{merged:?}");
        assert!(
            merged[0].contains("<subagent_results count=\"2\" tokens=\""),
            "{}",
            merged[0]
        );
        assert!(
            merged[0].contains("wf_y") && merged[0].contains("ab"),
            "{}",
            merged[0]
        );
        reset_all();
    }

    #[test]
    fn join_batches_concurrent_background_completions() {
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = lock();
        reset_all();
        while crate::core::extensions::take_pending_ui().is_some() {}
        with_state(|st| {
            st.cfg = SubagentConfig {
                join: JoinMode::Smart,
                ..Default::default()
            }
        });

        // A 慢、B 快：B 完成时 A 仍在跑 → 攒批；A 完成 → 一次性投递
        let slow = SeenSpec {
            hold_ms: 200,
            ..Default::default()
        };
        let fast = SeenSpec {
            hold_ms: 10,
            ..Default::default()
        };
        let ctx_slow = fake_ctx(slow, Arc::new(AtomicBool::new(false)));
        let ctx_fast = fake_ctx(fast, Arc::new(AtomicBool::new(false)));
        rt().block_on(async {
            let a = dispatch(&ctx_slow, &ty("E"), req("slow", true))
                .await
                .unwrap();
            let b = dispatch(&ctx_fast, &ty("E"), req("fast", true))
                .await
                .unwrap();
            wait(&a.id).await;
            wait(&b.id).await;

            // 批处理在最后一个完成的任务里投递（finish 之后），稍等其落地
            let mut conts: Vec<String> = Vec::new();
            for _ in 0..50 {
                while let Some(r) = crate::core::extensions::take_pending_ui() {
                    if let ExtensionUiRequest::Continuation { message } = r {
                        conts.push(message.text());
                    }
                }
                if !conts.is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert_eq!(
                conts.len(),
                1,
                "两个并发后台代理应合并为一条通知: {conts:?}"
            );
            assert!(
                conts[0].contains("<subagent_results count=\"2\" tokens=\""),
                "{}",
                conts[0]
            );
            assert!(conts[0].contains("echo:slow"), "{}", conts[0]);
            assert!(conts[0].contains("echo:fast"), "{}", conts[0]);
        });

        // async 模式：逐个通知
        with_state(|st| {
            st.cfg = SubagentConfig {
                join: JoinMode::Async,
                ..Default::default()
            }
        });
        while crate::core::extensions::take_pending_ui().is_some() {}
        let slow = SeenSpec {
            hold_ms: 100,
            ..Default::default()
        };
        let ctx_slow = fake_ctx(slow, Arc::new(AtomicBool::new(false)));
        let ctx_fast = fake_ctx(SeenSpec::default(), Arc::new(AtomicBool::new(false)));
        rt().block_on(async {
            let a = dispatch(&ctx_slow, &ty("E"), req("slow", true))
                .await
                .unwrap();
            let b = dispatch(&ctx_fast, &ty("E"), req("fast", true))
                .await
                .unwrap();
            wait(&a.id).await;
            wait(&b.id).await;
            let mut count = 0;
            for _ in 0..50 {
                while let Some(r) = crate::core::extensions::take_pending_ui() {
                    if matches!(r, ExtensionUiRequest::Continuation { .. }) {
                        count += 1;
                    }
                }
                if count >= 2 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert_eq!(count, 2, "async 模式应逐个通知");
        });
        reset_all();
    }

    /// `.output` 转写：开启时落 JSONL 并把路径写进记录；关闭时不建文件。
    #[test]
    fn output_transcript_writes_jsonl_and_records_path() {
        let _g = lock();
        reset_all();
        let seen = SeenSpec::default();
        let ctx = fake_ctx(seen, Arc::new(AtomicBool::new(false)));
        rt().block_on(async {
            let mut r = req("ot", false);
            r.output_transcript = true;
            let d = dispatch(&ctx, &ty("E"), r).await.unwrap();
            let rec = record(&d.id).unwrap();
            let path = rec.output_path.clone().expect("应记录 .output 路径");
            assert!(path.ends_with(&format!("{}.output", d.id)), "{path}");
            // sink 里已经写过头行（假 runner 不发事件，故至少 1 行）
            let text = std::fs::read_to_string(&path).unwrap();
            assert!(text.contains("agent_start"), "{text}");
            let _ = std::fs::remove_file(&path);

            // 关闭时：无路径、无文件
            let d = dispatch(&ctx, &ty("E"), req("off", false)).await.unwrap();
            assert!(record(&d.id).unwrap().output_path.is_none());
        });
        reset_all();
    }

    /// frontmatter 派生的 spec 字段（disallowed_tools / isolated / output_transcript）透传到 spec。
    #[test]
    fn spawn_request_spec_fields_reach_the_spec() {
        let t = ty("E");
        let mut r = req("go", false);
        r.disallowed_tools = Some(vec!["bash".to_string()]);
        r.isolated = true;
        r.output_transcript = true;
        let spec = build_spec(&t, &r, "/tmp", None, None, 2, 0, 5);
        assert_eq!(spec.disallowed_tools, vec!["bash".to_string()]);
        assert!(spec.isolated);
        // output_transcript 由 manager 在 dispatch 时开文件，不进 spec（核心不解释该字段）
    }

    #[test]
    fn resume_rejects_non_terminal_agent() {
        let _g = lock();
        reset_all();
        with_state(|st| st.running_bg = st.cfg.max_concurrent);
        let seen = SeenSpec::default();
        let ctx = fake_ctx(seen, Arc::new(AtomicBool::new(false)));
        rt().block_on(async {
            let d = dispatch(&ctx, &ty("E"), req("t", true)).await.unwrap();
            let mut r = req("again", false);
            r.resume = Some(d.id.clone());
            let err = dispatch(&ctx, &ty("E"), r).await.unwrap_err();
            assert!(err.contains("still queued"), "{err}");
        });
        reset_all();
    }

    /// resume 一个**后台**已结算 agent 必须真的重新跑起来。
    ///
    /// 回归点：`insert_run_record` 的 resume 分支曾把状态直接置 `Running`，
    /// 而后台启动入口 `start_background` 只认 `Queued` → 静默不启动：记录永远停在
    /// Running、runner 从未被调、主页也就看不到任何输出（用户报告的“resume 没反应”）。
    #[test]
    fn background_resume_actually_restarts_the_agent() {
        let _g = lock();
        reset_all();
        rt().block_on(async {
            let seen = SeenSpec::default();
            let ctx = fake_ctx(seen, Arc::new(AtomicBool::new(false)));

            let d = dispatch(&ctx, &ty("Explore"), req("first", true))
                .await
                .unwrap();
            let first = wait(&d.id).await.expect("first run settles");
            assert_eq!(first.status, AgentStatus::Completed);
            assert_eq!(first.result.as_deref(), Some("echo:first"));

            // resume 要求目标有落盘会话
            with_state(|st| {
                if let Some(r) = st.records.get_mut(&d.id) {
                    r.session_path = Some("/tmp/s.jsonl".to_string());
                }
            });

            let mut resume = req("second", true);
            resume.resume = Some(d.id.clone());
            let dr = dispatch(&ctx, &ty("Explore"), resume).await.unwrap();
            assert_eq!(dr.id, d.id, "resume 复用原记录 id");

            // 关键断言：后台 resume 的 runner 必须真的重跑并产出新结果。
            // 用超时兜底：resume 静默不启动时 `wait` 永不返回，这里要的是可读的失败而不是挂死。
            let resumed = tokio::time::timeout(std::time::Duration::from_secs(5), wait(&d.id))
                .await
                .expect("resumed run must start/settle in time")
                .expect("resumed record");
            assert_eq!(resumed.status, AgentStatus::Completed);
            assert_eq!(resumed.result.as_deref(), Some("echo:second"));
        });
        reset_all();
    }

    #[test]
    fn unknown_key_errors_are_explicit_and_warnings_dedupe() {
        let _g = lock();
        reset_all();
        assert!(record("nope").is_none());
        assert!(
            steer("nope", "hi")
                .unwrap_err()
                .contains("unknown agent id")
        );
        assert!(stop("nope").unwrap_err().contains("unknown agent id"));
        assert!(note_warning("a|b"));
        assert!(!note_warning("a|b"));
        assert!(note_warning("a|c"));
        reset_all();
    }

    #[test]
    fn reset_all_clears_everything_and_unblocks_waiters() {
        let _g = lock();
        reset_all();
        with_state(|st| st.running_bg = st.cfg.max_concurrent);
        let seen = SeenSpec::default();
        let ctx = fake_ctx(seen, Arc::new(AtomicBool::new(false)));
        rt().block_on(async {
            let d = dispatch(&ctx, &ty("E"), req("t", true)).await.unwrap();
            reset_all();
            assert!(record(&d.id).is_none());
            assert!(list().is_empty());
            assert!(widget_lines().is_empty());
        });
    }

    #[test]
    fn widget_lines_follow_config_and_show_badges() {
        let _g = lock();
        reset_all();
        let mut t = ty("auditor");
        t.display_name = "Auditor".to_string();
        t.color = Some("teal".to_string());
        // 后台 + 长停留：让记录停在运行中，收尾即隐去的规则才不会把它从 dock 拿走。
        let ctx = fake_ctx(
            SeenSpec {
                hold_ms: 100_000,
                ..Default::default()
            },
            Arc::new(AtomicBool::new(false)),
        );
        rt().block_on(async {
            let d = dispatch(&ctx, &t, req("hi", true)).await.unwrap();
            let rec = record(&d.id).unwrap();
            assert_eq!(rec.color.as_deref(), Some("teal"), "类型色随记录传递");
        });

        let lines = widget_lines();
        assert_eq!(lines.len(), 2, "标题 + 一条运行中的记录");
        assert!(widget_text(&lines[1]).contains("Auditor"));
        let badge = lines[1]
            .iter()
            .find(|s| s.text.contains("Auditor"))
            .expect("badge span");
        assert_eq!(badge.fallback.as_ref(), "#008080", "teal → 主题外 hex");

        // 面板把 widget 关掉 → 不再占位
        let mut off = SubagentConfig::default();
        crate::extensions::subagent::config::apply_panel_choice(&mut off, "widget", "off").unwrap();
        with_state(|st| st.cfg = off);
        assert!(widget_lines().is_empty());
        reset_all();
    }

    /// 最后一个代理收尾即从 dock 隐去（面板自动收起），但记录不删
    /// （`/agents` 面板与 `@handle` resume 照样能看）；水位只盖住收尾前的记录。
    #[test]
    fn settled_records_leave_the_dock_immediately() {
        let _g = lock();
        reset_all();
        let ctx = fake_ctx(SeenSpec::default(), Arc::new(AtomicBool::new(false)));
        rt().block_on(async {
            dispatch(&ctx, &ty("E"), req("hi", false)).await.unwrap();
        });
        assert!(widget_lines().is_empty(), "全部收尾后 dock 不再占位");
        assert_eq!(list().len(), 1, "记录保留：/agents 与 resume 不受影响");

        // 水位不影响之后新派的记录：还在跑的代理照常显示
        let ctx = fake_ctx(
            SeenSpec {
                hold_ms: 100_000,
                ..Default::default()
            },
            Arc::new(AtomicBool::new(false)),
        );
        rt().block_on(async {
            dispatch(&ctx, &ty("E"), req("again", true)).await.unwrap();
        });
        assert_eq!(widget_lines().len(), 2, "新派的运行中记录照常占 dock");
        reset_all();
    }

    /// 还有活儿在跑时不隐去：用户在等结果，dock 应该继续占位。
    #[test]
    fn dismiss_finished_is_a_noop_while_work_is_live() {
        let _g = lock();
        reset_all();
        with_state(|st| st.running_bg = 1);
        dismiss_finished();
        assert_eq!(
            with_state(|st| st.dismissed_before_ms),
            0,
            "有后台代理在跑 → 水位不动"
        );

        with_state(|st| st.running_bg = 0);
        with_state(|st| st.queue.push_back("queued-id".to_string()));
        dismiss_finished();
        assert_eq!(
            with_state(|st| st.dismissed_before_ms),
            0,
            "有排队记录 → 水位不动"
        );
        reset_all();
    }

    #[test]
    fn foreground_limit_serializes_foreground_runs() {
        let _g = lock();
        reset_all();
        // 通过**配置文件**设限额：`config()` 在别的任务里可能因为 mtime 变化重载，
        // 若只改内存就会被那次重载清成默认（无限额），测试随之假失败。
        let cfg = config::SubagentConfig {
            foreground_max_concurrent: 1,
            ..Default::default()
        };
        config::write_config(&config::config_path(), &cfg);
        reload_config();
        let seen = SeenSpec {
            hold_ms: 200,
            ..Default::default()
        };
        let ctx = fake_ctx(seen, Arc::new(AtomicBool::new(false)));
        rt().block_on(async {
            let a = tokio::spawn({
                let ctx = ctx.clone();
                async move { dispatch(&ctx, &ty("E"), req("a", false)).await.unwrap() }
            });
            // 第二个前台调用必须等第一个跑完；期间不占额度就无法开始
            tokio::time::sleep(Duration::from_millis(30)).await;
            let b = tokio::spawn({
                let ctx = ctx.clone();
                async move { dispatch(&ctx, &ty("E"), req("b", false)).await.unwrap() }
            });
            tokio::time::sleep(Duration::from_millis(30)).await;
            let (running, live): (usize, Vec<(String, String, bool)>) = with_state(|st| {
                (
                    st.running_fg,
                    st.records
                        .values()
                        .filter(|r| !r.status.is_terminal())
                        .map(|r| (r.id.clone(), r.status.as_str().to_string(), r.background))
                        .collect(),
                )
            });
            let limit = with_state(|st| st.cfg.foreground_max_concurrent);
            assert_eq!(
                running, 1,
                "前台额度=1 时同时只有 1 个在跑 (live={live:?}, limit={limit})"
            );
            a.await.unwrap();
            b.await.unwrap();
            assert_eq!(with_state(|st| st.running_fg), 0, "收尾后释放额度");
        });
        config::write_config(&config::config_path(), &config::SubagentConfig::default());
        reload_config();
        reset_all();
    }
}
