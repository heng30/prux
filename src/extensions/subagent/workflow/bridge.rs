//! 宿主桥：rquickjs 沙箱驱动循环。
//!
//! # 为什么不直接 await JS 的 Promise
//!
//! `AsyncContext::async_with` 要求其 closure 的输出 future 是 `Send`（`ParallelSend`），
//! 而 rquickjs 的 `PromiseFuture` / `Function` / `Value` 都是 `!Send`（内部持 `Rc`）。
//! 因此宿主**不 await 脚本的 Promise**：脚本体外包一层，结束/抛错时调宿主注入的 `__done(json)`；
//! Rust 只 await 一个 `tokio` 通道。`Ctx` 本身 `unsafe impl Send`，
//! 所有 JS 值都在同步分支里取用后立即丢弃，故 closure future 始终 `Send`。
//!
//! # 驱动
//!
//! `WithFuture` 在 closure future 挂起时会自动跑 JS 任务队列（microtask），
//! 所以宿主每处理完一批 `agent` 结果并 `__settle` 后，JS 的续体在下一轮 poll 里推进。
//! `__dispatch` 入队 + `Notify` 唤醒；`__progress` 收集进度；`__budgetSpent` 读累计。

use super::{
    journal::{self, Journal, JournalEntry, JournalKeyInput},
    progress::EntryState,
    task::ProgressLog,
};
use crate::{extensions::util::truncate_chars, utils::time::now_ms};
use futures_util::future::BoxFuture;
use rquickjs::{AsyncContext, AsyncRuntime, Ctx, Function};
use serde_json::Value;
use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::{
    Notify, Semaphore,
    mpsc::{UnboundedSender, unbounded_channel},
};

/// 执行一次 `agent()`（宿主注入）。
pub type AgentRunner = Arc<dyn Fn(AgentRequest) -> BoxFuture<'static, AgentReport> + Send + Sync>;

/// "这个 schema 能用吗"（`Err` 的消息会直接给用户看）。
pub type SchemaGate = Arc<dyn Fn(&Value) -> Result<(), String> + Send + Sync>;

/// "这段文本还满足这个 schema 吗"。
pub type SchemaTextGate = Arc<dyn Fn(&Value, &str) -> Result<(), String> + Send + Sync>;

/// `workflow()` 解析器：入参是 glue 传来的 `{name?, scriptPath?}`。解析失败应由脚本侧捕获。
pub type ResolveWorkflowFn = dyn Fn(&Value) -> Result<ResolvedWorkflow, String> + Send + Sync;

/// JS 侧 glue：注入 `agent` / `phase` / `workflow` 等全局。
const GLUE: &str = include_str!("glue.js");

/// `workflow()` 的嵌套上限
const NESTED_CAP: u64 = 256;

/// 确定性前奏：
/// 在脚本之前于全局词法作用域求值：`Date` 被 `const` 遮蔽、`Math.random` 被改写，
/// 让 `Date.now()` / `Math.random()` / 无参 `new Date()` 都抛错（否则 journal 重放会漂移）；
/// 另外禁用 `eval` / `new Function`。
const DETERMINISM_PRELUDE: &str = concat!(
    "const __wfDie = function (what) {",
    " throw new Error(what + \" is unavailable in workflow scripts (breaks resume).",
    " Stamp results after the workflow returns, or pass timestamps via `args`.\");",
    " };",
    "const Date = (function () {",
    " const RealDate = globalThis.Date;",
    " RealDate.now = function () { return __wfDie(\"Date.now()\"); };",
    " Math.random = function () { return __wfDie(\"Math.random()\"); };",
    " return class WorkflowDate extends RealDate {",
    " constructor() { if (arguments.length === 0) __wfDie(\"new Date()\"); super(...arguments); }",
    " };",
    "})();",
    "globalThis.eval = function () { __wfDie(\"eval()\"); };",
    "globalThis.Function = function () { __wfDie(\"new Function()\"); };"
);

/// 兜底墙钟上限（防脚本既不调 `__done` 也不被中断）。
const MAX_WALL_MS: u64 = 30 * 60 * 1000;

/// 取消/超时检查间隔。
const TICK: Duration = Duration::from_millis(50);

/// 跨工作流边界的体积上界（args / 返回值序列化后的字节数）。
const MAX_BOUNDARY_BYTES: usize = 524_288;

/// 一次 `agent()` 调用（已从 JS payload 解析）。
#[derive(Debug, Clone, Default)]
pub struct AgentRequest {
    /// 任务正文。
    pub prompt: String,
    /// `opts.label`：进度与 `resume` 用的标签。
    pub label: Option<String>,
    /// `opts.model`：本次调用的型号覆盖。
    pub model: Option<String>,
    /// `opts.agentType`（缺省 = general-purpose）。
    pub agent_type: Option<String>,
    /// `opts.effort`：思考档位。
    pub effort: Option<String>,
    /// `opts.isolation`：仅 `"worktree"` 合法（值已在 JS 侧校验）
    pub isolation: Option<String>,
    /// `opts.schema`：结构化输出约束（有则注入 `StructuredOutput` 工具）。
    pub schema: Option<Value>,
    /// 显式 `opts.phase` 或 ambient `phase()` 的下标（JS 侧算好）
    pub phase_index: Option<u64>,
    /// 阶段标题（与 `phase_index` 配对）。
    pub phase_title: Option<String>,
    /// `opts.gate`：子代理跑完后要跑的命令；非零退出即该 agent 失败
    pub gate: Option<String>,
    /// `opts.resume`：续跑先前这个 label 下跑过的 child（由 runner 解析成记录 id）
    pub resume: Option<String>,
    /// 本次尝试的取消标志：runner 用它当 child 的 `parent_abort`
    /// （这样"跳过/重跑/掐 run"能真的停掉那个 child，而不是只不再启动新的）。
    pub abort: Arc<AtomicBool>,
    /// 派发回调：runner 在拿到记录 id 后立刻调用它。桥据此在 agent **还在跑**时
    /// 就把 `recordId` 写进进度条目，检查器才能在运行中 `c` 打开它的会话。
    pub on_spawn: SpawnHook,
}

/// 子代理产出。
#[derive(Debug, Clone)]
pub enum AgentOutcome {
    /// 无 `schema`：最终文本
    Text(String),
    /// 有 `schema`：经 `StructuredOutput` 工具返回的结构化结果
    Structured(Value),
}

/// 一次 `agent()` 结算时夹回来的元数据（进度条目用）。
///
/// 除了 `record_id`（runner 在**派发时**就通过 [`AgentRequest::on_spawn`] 回传，
/// 运行中的行也有）之外，其余值只在**结算时**补一次。
#[derive(Debug, Clone, Default)]
pub struct AgentMeta {
    /// 管理器的记录 id
    pub record_id: Option<String>,
    /// 实际派发的类型名。
    pub agent_type: Option<String>,
    /// 输入的 tokens 数。
    pub tokens: Option<u64>,
    /// 输出 token（计入 `budget.spent()`）。
    pub output_tokens: Option<u64>,
    /// 工具调用次数。
    pub tool_calls: Option<u64>,
    /// 运行时长。
    pub duration_ms: Option<u64>,
    /// 被用户跳过。
    pub skipped: bool,
    /// 未执行（被拦 / 依赖失败）。
    pub blocked: bool,
}

/// runner 在**派发时**拿到记录 id 后立刻回调一次（见 [`AgentRequest::on_spawn`]）。
///
/// 用 newtype 而不是裸 `Option<Arc<dyn Fn>>`，是为了保住 `AgentRequest` 的 `Debug`/`Default`。
#[derive(Clone, Default)]
pub struct SpawnHook(Option<Arc<dyn Fn(String) + Send + Sync>>);

impl SpawnHook {
    /// 包一个回调。
    pub fn new(f: impl Fn(String) + Send + Sync + 'static) -> Self {
        SpawnHook(Some(Arc::new(f)))
    }

    /// 取内部回调（交给 `manager::dispatch_with_id`）。
    pub fn callback(&self) -> Option<Arc<dyn Fn(String) + Send + Sync>> {
        self.0.clone()
    }
}

impl std::fmt::Debug for SpawnHook {
    /// 只打印是否包了回调，不暴露闭包本身（闭包无法 Debug，也不该进日志）。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("SpawnHook").field(&self.0.is_some()).finish()
    }
}

/// 一次 `agent()` 的结果：结局 + 元数据。
///
/// 结局用内层 `Result` 而不是外层：这样**失败也带元数据**
/// （被停掉的子代理要标 `skipped`，而这只有 runner 知道），
/// 而外层 future 本身不会 reject。
#[derive(Debug, Clone)]
pub struct AgentReport {
    /// 结局：成功文本/结构化，或失败原因。
    pub result: Result<AgentOutcome, String>,
    /// 结算时夹回的元数据。
    pub meta: AgentMeta,
}

impl AgentReport {
    /// 构造成功结局：结果为纯文本，元数据留空。
    pub fn text(text: impl Into<String>) -> Self {
        AgentReport {
            result: Ok(AgentOutcome::Text(text.into())),
            meta: AgentMeta::default(),
        }
    }

    /// 构造成功结局：结果为经 `StructuredOutput` 工具产出的结构化 JSON。
    pub fn structured(value: Value) -> Self {
        AgentReport {
            result: Ok(AgentOutcome::Structured(value)),
            meta: AgentMeta::default(),
        }
    }

    /// 构造失败结局：`message` 交回脚本侧（脚本会看到 `null`），原因保留在进度条目上。
    pub fn failed(message: impl Into<String>) -> Self {
        AgentReport {
            result: Err(message.into()),
            meta: AgentMeta::default(),
        }
    }

    /// 链式挂上结算元数据（消耗并返回 self）。
    pub fn with_meta(mut self, meta: AgentMeta) -> Self {
        self.meta = meta;
        self
    }
}

/// 单次运行的配额。
#[derive(Debug, Clone, Copy)]
pub struct WorkflowLimits {
    /// 并发子代理上限（`max(1, min(16, cpus-2))`）
    pub max_concurrency: usize,
    /// 整次运行累计子代理上限
    pub max_agents: u64,
    /// 单次 `parallel()`/`pipeline()` 的 item 上限
    pub max_items: usize,
}

impl Default for WorkflowLimits {
    /// 默认配额：并发取 `min(16, cpus-2)` 且至少 1，agents 1000、items 4096。
    fn default() -> Self {
        let cpus = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(2);
        WorkflowLimits {
            max_concurrency: 16usize.min(cpus.saturating_sub(2)).max(1),
            max_agents: 1000,
            max_items: 4096,
        }
    }
}

/// 脚本运行结果。
#[derive(Debug, Clone)]
pub struct WorkflowOutcome {
    /// 脚本 `return` 的 JSON 值（`null` = 返回 undefined）
    pub value: Value,
    /// 累计调用的子代理数
    pub agent_count: u64,
}

/// schema 相关的两个闸门，由扩展注入——kernel 不该认识 JSON Schema。
///
/// 返回 `Result<(), String>` 而非编译产物，则 kernel 侧不需要任何 `CompiledSchema` 类型。
#[derive(Clone)]
pub struct SchemaHooks {
    /// 这个 schema 能不能用（spawn 之前跑；`Err` → 整次运行致命）
    pub compile: SchemaGate,
    /// 重放出来的文本是否仍满足这个 schema（`(schema, text)`）——journal 可能被手改，
    /// 或被截断的末行留下半截内容，那种条目不能当成"上次成功"交回脚本。
    pub check_text: SchemaTextGate,
}

impl SchemaHooks {
    /// 只有一件校验能力时的便利构造（测试用）。
    #[cfg(test)]
    pub fn compile_only(compile: SchemaGate) -> Self {
        SchemaHooks {
            compile,
            check_text: Arc::new(|_, _| Ok(())),
        }
    }
}

/// 一次运行的控制面，交给调用方（运行注册表 / 检查器）驱动。
///
/// # 三件事的语义
///
/// - `pause()` 只停止**启动**新 agent；已经在跑的不动。
///   把一次模型回合从中间掐掉等于扔掉它已经花掉的一切，而且没有地方把上下文还给它。
/// - `skip(index)` 放弃那一个 agent：它的 `agent()` 调用返回 `null`（与终态失败一模一样），
///   进度行画成 skipped，journal 记成失败（否则下次恢复会把它当成"上次成功"重放）。
///   对正在跑的立即生效；对卡在暂停/并发上限后面的，等它排到队首时生效。
/// - `retry(index)` 只在那一个 agent **正在跑**时有效——那是唯一的窗口：调用一旦结算，
///   它的值就已经是脚本的了，再跑一次也没有地方交结果。
#[derive(Clone)]
pub struct WorkflowControl {
    /// 暂停：停止**启动**新 agent（已在跑的不动）。
    pause: Arc<dyn Fn() + Send + Sync>,
    /// 继续：恢复启动，并叫醒全部卡在暂停上的调用。
    resume: Arc<dyn Fn() + Send + Sync>,
    /// 查询当前是否处于暂停态。
    is_paused: Arc<dyn Fn() -> bool + Send + Sync>,
    /// 放弃第 `index` 个 agent；已结算或不存在则返回 `false`。
    skip: Arc<dyn Fn(u64) -> bool + Send + Sync>,
    /// 重跑第 `index` 个 agent；仅当它**正在跑**时有效，否则返回 `false`。
    retry: Arc<dyn Fn(u64) -> bool + Send + Sync>,
}

impl WorkflowControl {
    /// 暂停启动新的 agent（已在跑的不受影响）。
    pub fn pause(&self) {
        (self.pause)()
    }
    /// 恢复启动，并叫醒全部卡在暂停上的调用。
    pub fn resume(&self) {
        (self.resume)()
    }
    /// 查询当前是否处于暂停态。
    pub fn is_paused(&self) -> bool {
        (self.is_paused)()
    }
    /// 放弃第 `index` 个 agent；它已经结算过则返回 `false`。
    pub fn skip(&self, index: u64) -> bool {
        (self.skip)(index)
    }
    /// 重跑第 `index` 个 agent；它不在跑则返回 `false`。
    pub fn retry(&self, index: u64) -> bool {
        (self.retry)(index)
    }
}

/// 一个活跃调用在共享表里的那一格。
struct LiveCall {
    /// 当前尝试的取消标志（也是一次 child 的 `parent_abort`）
    abort: Arc<AtomicBool>,
    /// 从暂停里叫醒它（`resume` 叫醒全部；`skip`/`retry` 叫醒那一个）
    wake: Arc<Notify>,
    /// 是否已经拿到并发槽位开跑（`retry` 只对已开跑的有效）
    started: bool,
    /// 已结算（之后的 skip/retry 都是 no-op）
    settled: bool,
    /// 待处理的意图
    intent: Option<Intent>,
}

/// 控制面对某个活跃调用下达的意图：跳过或重跑。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Intent {
    /// 放弃这个 agent（脚本拿到 `null`，行标 skipped）。
    Skip,
    /// 重跑这个 agent（仅对已开跑的有效）。
    Retry,
}

/// 控制面的共享状态。
struct Control {
    /// 是否暂停启动新 agent（已在跑的不受影响）。
    paused: AtomicBool,
    /// 活跃调用表：`index → LiveCall`（skip/retry/拾 run 都靠它）。
    live: Mutex<HashMap<u64, LiveCall>>,
}

impl Control {
    /// 新建共享控制状态：初始未暂停、活跃调用表为空。
    fn new() -> Arc<Self> {
        Arc::new(Control {
            paused: AtomicBool::new(false),
            live: Mutex::new(HashMap::new()),
        })
    }

    /// 叫醒全部活跃调用（`resume` 用），让它们重新检查暂停标志。
    fn wake_all(&self) {
        for call in self.live.lock().unwrap().values() {
            call.wake.notify_waiters();
        }
    }
}

/// 运行输入（全部 `'static`，便于搬到独立线程）。
pub struct WorkflowInputs {
    /// 已经过 `meta` 剥离的脚本体（`export ` 已替换为空格）
    pub body: String,
    /// `args` 全局（JSON 值）
    pub args: Option<Value>,
    /// 配额（agents / items / 并发）。
    pub limits: WorkflowLimits,
    /// 跑一次 `agent()` 的回调（工具层实现）。
    pub runner: AgentRunner,
    /// 外部取消（父级 abort / 工具中止）
    pub cancel: Arc<AtomicBool>,
    /// 本次运行累计 output token（`budget.spent()`）
    pub spent: Arc<AtomicU64>,
    /// 共享进度日志（run 持有、界面每帧读；kernel 只往里追加）
    pub progress: ProgressLog,
    /// `agent({schema})` 的 schema 闸门（spawn 之前跑、重放时再校验）
    pub schema: Option<SchemaHooks>,
    /// 可重放的 journal（`resumeFromRunId`）；`None` = 既不重放也不记录
    pub journal: Option<Journal>,
    /// 控制面交回点：运行开始前调一次，把 [`WorkflowControl`] 交给调用方
    pub control_sink: Option<Arc<dyn Fn(WorkflowControl) + Send + Sync>>,
    /// `workflow()` 的脚本解析器（工具层用 `saved` 实现）；`None` = 不提供该全局。
    pub resolve_workflow: Option<Arc<ResolveWorkflowFn>>,
}

/// `workflow()` 的脚本解析结果（`meta` 已剥离的脚本体 + 显示名）。
#[derive(Clone, Debug)]
pub struct ResolvedWorkflow {
    /// 工作流显示名。
    pub name: String,
    /// `meta` 剥离后的脚本体。
    pub body: String,
}

/// 跨嵌套运行共享的状态
struct RunShared {
    /// 代理计数器（配额与 `wf-agent-N` 序号共用；嵌套共享同一份）
    count: Arc<AtomicU64>,
    /// 阶段序号分配器（跨嵌套脚本唯一，避免 UI 里两张不同的阶段树撞索引）
    phase: Arc<AtomicU64>,
    /// 已发起的 `workflow()` 调用数
    nested: Arc<AtomicU64>,
    /// `workflow()` 解析器（嵌套运行时按需 clone）。
    resolve: Option<Arc<ResolveWorkflowFn>>,
    /// 当前脚本所处嵌套层数（顶层 0；子脚本 1，再调 `workflow()` 报错）
    depth: u32,
}

/// 宿主发往 JS 侧的一次方法调用（`agent` / `workflow`），入队后由脚本线程取走执行。
struct Dispatch {
    /// JS 侧调用 id（settle 时原样回传）。
    id: u64,
    /// 宿主方法名（`agent` / `workflow`）。
    method: String,
    /// 调用载荷（JSON 字符串）。
    json: String,
}

/// JS 侧回传的调用结果，宿主据此唤醒并结算对应的 agent 调用。
struct Settle {
    /// 对应 [`Dispatch::id`]。
    id: u64,
    /// 是否成功（false 时脚本侧 reject）。
    ok: bool,
    /// 交回脚本的结果（JSON 编码）。
    json: String,
}

/// 解析 `agent` 的 JS payload。
fn parse_agent_request(json: &str) -> Result<AgentRequest, String> {
    let v: Value = serde_json::from_str(json).map_err(|e| format!("bad agent payload: {e}"))?;
    let obj = v.as_object().ok_or("bad agent payload: not an object")?;
    let prompt = obj
        .get("prompt")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();

    let s = |k: &str| obj.get(k).and_then(|x| x.as_str()).map(|x| x.to_string());

    Ok(AgentRequest {
        prompt,
        label: s("label"),
        model: s("model"),
        agent_type: s("agentType"),
        effort: s("effort"),
        isolation: s("isolation"),
        schema: obj.get("schema").filter(|x| !x.is_null()).cloned(),
        phase_index: obj.get("phaseIndex").and_then(Value::as_u64),
        phase_title: obj
            .get("phaseTitle")
            .and_then(Value::as_str)
            .map(str::to_string),
        gate: s("gate"),
        resume: s("resume"),
        abort: Arc::new(AtomicBool::new(false)),
        on_spawn: SpawnHook::default(),
    })
}

/// 首行、去空白——agent 缺省显示名。
/// 工具侧记 `agent({ resume })` 的 label → 记录 id 表时要与进度条目用同一个口径。
pub(crate) fn derived_label(prompt: &str) -> String {
    let line = prompt.lines().next().unwrap_or_default().trim();
    if line.is_empty() {
        return "agent".to_string();
    }

    truncate_chars(line, 60)
}

/// 取文本前 120 个字符作为进度条目里的预览。
fn preview(text: &str) -> String {
    truncate_chars(text, 120)
}

/// 给某个 index 打一个意图（skip/retry）：**只对还没结算的**有效。
///
/// `retry` 只对**已开跑**的有效（还没开跑的直接重跑没有意义——它本来就会跑）。
fn mark_intent(ctl: &Arc<Control>, index: u64, intent: Intent) -> bool {
    let mut live = ctl.live.lock().unwrap();
    let Some(call) = live.get_mut(&index) else {
        return false;
    };

    if call.settled || (intent == Intent::Retry && !call.started) {
        return false;
    }

    call.intent = Some(intent);

    // 叫醒暂停中的等待 + 停掉正在跑的 child（跳过/重跑都要它让出位置）
    call.wake.notify_waiters();
    call.abort.store(true, Ordering::Relaxed);

    true
}

/// 取走这一格的意图（取即清）。
fn take_intent(ctl: &Arc<Control>, index: u64) -> Option<Intent> {
    ctl.live
        .lock()
        .unwrap()
        .get_mut(&index)
        .and_then(|c| c.intent.take())
}

/// 把该 index 标记为已开跑（拿到并发槽位后调用）；`retry` 只对已开跑的有效。
fn mark_started(ctl: &Arc<Control>, index: u64) {
    if let Some(call) = ctl.live.lock().unwrap().get_mut(&index) {
        call.started = true;
    }
}

/// 把该 index 标记为已结算，之后的 skip/retry 都成为 no-op。
fn mark_settled(ctl: &Arc<Control>, index: u64) {
    if let Some(call) = ctl.live.lock().unwrap().get_mut(&index) {
        call.settled = true;
    }
}

/// 放弃一个 agent：脚本拿到 `null`（与终态失败一样），行画成 skipped，journal 记成失败
/// （否则下次恢复会把它当成"上次成功"重放）。
#[allow(clippy::too_many_arguments)]
fn settle_skipped(
    progress: &ProgressLog,
    entry: &mut Value,
    index: u64,
    journal_path: &Option<PathBuf>,
    journal_key: &str,
    tx: &UnboundedSender<Settle>,
    settle_id: u64,
) {
    entry["state"] = serde_json::json!(EntryState::Error.as_str());
    entry["skipped"] = serde_json::json!(true);
    entry["error"] = serde_json::json!("Skipped by user.");
    entry["lastProgressAt"] = serde_json::json!(now_ms());
    push_progress(progress, entry.clone());

    record_journal(
        journal_path.as_deref(),
        &JournalEntry {
            index,
            key: journal_key.to_string(),
            ok: false,
            text: None,
            resumed: false,
        },
    );

    _ = tx.send(Settle {
        id: settle_id,
        ok: true,
        json: "null".to_string(),
    });
}

/// 记一条 journal（没有 journal / 写不进去都只是少一次将来的恢复）。
fn record_journal(path: Option<&Path>, entry: &JournalEntry) {
    if let Some(path) = path {
        journal::append(path, entry);
    }
}

/// 往共享进度日志追加一条（run 与界面都看它）。
fn push_progress(log: &ProgressLog, entry: Value) {
    log.lock().unwrap().push(entry);
}

/// 一个 agent 进度条目的**不变量部分**（每次跃迁都带全量重发：折叠按 index last-write-wins）。
fn progress_entry_base(index: u64, req: &AgentRequest) -> Value {
    let label = req
        .label
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| derived_label(&req.prompt));

    let mut entry = serde_json::json!({
        "type": "workflow_agent",
        "index": index,
        "label": label,
        "state": EntryState::Start.as_str(),
        "agentId": format!("wf-agent-{index}"),
        "agentType": req.agent_type.clone().unwrap_or_else(|| "general-purpose".to_string()),
        "prompt": req.prompt.clone(),
        "promptPreview": preview(&req.prompt),
        "queuedAt": now_ms(),
    });

    if let Some(p) = req.phase_index {
        entry["phaseIndex"] = serde_json::json!(p);
    }
    if let Some(t) = req.phase_title.as_ref() {
        entry["phaseTitle"] = serde_json::json!(t);
    }
    if let Some(m) = req.model.as_ref() {
        entry["model"] = serde_json::json!(m);
    }
    entry
}

/// 把结算元数据写进条目（缺的值不写，保留上一条的）。
fn apply_meta(entry: &mut Value, meta: &AgentMeta) {
    let set_str = |entry: &mut Value, key: &str, value: &Option<String>| {
        if let Some(v) = value.as_ref() {
            entry[key] = serde_json::json!(v);
        }
    };
    set_str(entry, "recordId", &meta.record_id);
    set_str(entry, "agentType", &meta.agent_type);
    if let Some(v) = meta.tokens {
        entry["tokens"] = serde_json::json!(v);
    }
    if let Some(v) = meta.tool_calls {
        entry["toolCalls"] = serde_json::json!(v);
    }
    if meta.skipped {
        entry["skipped"] = serde_json::json!(true);
    }
    if meta.blocked {
        entry["blocked"] = serde_json::json!(true);
    }
}

/// 把脚本返回值包装成会调 `__done` 的 async IIFE。
///
/// 在报告成功前调 `__checkUnawaited()`：
/// 脚本里“丢了”的 `agent()`（既未 await 也未 return）会让整次运行失败——否则那次启动会静默消失。
fn wrap_body(body: &str) -> String {
    format!(
        "(async () => {{\n  try {{\n    const __v = await (async () => {{\n{body}\n    }})();\n    const __un = (typeof __checkUnawaited === 'function') ? __checkUnawaited() : null;\n    if (__un) {{ __done(JSON.stringify({{ ok: false, message: __un, fatal: false }})); return; }}\n    if (__v !== undefined && typeof __checkBoundary === 'function') {{ __checkBoundary(__v, 'the workflow result'); }}\n    __done(JSON.stringify({{ ok: true, value: __v === undefined ? null : __v }}));\n  }} catch (e) {{\n    __done(JSON.stringify({{ ok: false, message: (e && e.message) ? e.message : String(e), fatal: !!(e && e.workflowFatal) }}));\n  }}\n}})();\n"
    )
}

/// 运行一个已通过 `meta` 校验的脚本体。
///
/// 顶层入口：分配跨嵌套共享的状态，再交给 [`run_script_inner`]。
pub async fn run_script(input: WorkflowInputs) -> Result<WorkflowOutcome, String> {
    let shared = Arc::new(RunShared {
        count: Arc::new(AtomicU64::new(0)),
        phase: Arc::new(AtomicU64::new(0)),
        nested: Arc::new(AtomicU64::new(0)),
        resolve: input.resolve_workflow.clone(),
        depth: 0,
    });
    run_script_inner(input, shared).await
}

/// 解析 `workflow()` payload 并跑子脚本；返回子脚本的 `return` 值。
///
/// `Err((message, fatal))`：`fatal` = 配额/未知 host 方法等“重试无意义”的错误。
#[allow(clippy::too_many_arguments)]
async fn run_nested(
    shared: &Arc<RunShared>,
    json: &str,
    runner: &AgentRunner,
    cancel: &Arc<AtomicBool>,
    spent: &Arc<AtomicU64>,
    progress: &ProgressLog,
    schema: &Option<SchemaHooks>,
    limits: WorkflowLimits,
) -> Result<Value, (String, bool)> {
    if shared.depth >= 1 {
        return Err((
            "workflow() cannot be nested more than one level deep — call the agents inline instead."
                .to_string(),
            false,
        ));
    }

    let Some(resolve) = shared.resolve.clone() else {
        return Err(("workflow() is not available in this run".to_string(), false));
    };
    let payload: Value =
        serde_json::from_str(json).map_err(|e| (format!("bad workflow payload: {e}"), false))?;
    let resolved = resolve(&payload).map_err(|e| (e, false))?;

    if shared.nested.fetch_add(1, Ordering::Relaxed) >= NESTED_CAP {
        return Err((
            format!("Workflow exceeded its cap of {NESTED_CAP} nested workflow() calls."),
            true,
        ));
    }

    progress.lock().unwrap().push(serde_json::json!({
        "type": "workflow_log",
        "message": format!("workflow \"{}\" started", resolved.name),
    }));

    let args = payload.get("args").filter(|v| !v.is_null()).cloned();
    let nested_shared = Arc::new(RunShared {
        count: shared.count.clone(),
        phase: shared.phase.clone(),
        nested: shared.nested.clone(),
        resolve: Some(resolve),
        depth: shared.depth + 1,
    });

    // 嵌套运行为自己的并发/Journal 负责；共享配额、阶段序号、取消与 progress，
    // 所以一个 `run` 的总量上限仍然成立、UI 也看得到子脚本的代理。
    let inputs = WorkflowInputs {
        body: resolved.body,
        args,
        limits,
        runner: runner.clone(),
        cancel: cancel.clone(),
        spent: spent.clone(),
        progress: progress.clone(),
        schema: schema.clone(),
        journal: None,
        control_sink: None,
        resolve_workflow: nested_shared.resolve.clone(),
    };

    // `Box::pin` 断开 `run_nested ⇄ run_script_inner` 的递归 future 类型环。
    match Box::pin(run_script_inner(inputs, nested_shared)).await {
        Ok(out) => Ok(out.value),
        Err(e) => Err((e, false)),
    }
}

/// 实际的运行主体（顶层与嵌套共用）。
async fn run_script_inner(
    input: WorkflowInputs,
    shared: Arc<RunShared>,
) -> Result<WorkflowOutcome, String> {
    let WorkflowInputs {
        body,
        args,
        limits,
        runner,
        cancel,
        spent,
        progress,
        schema,
        journal,
        control_sink,
        // 顶层与嵌套共享的解析器已收进 `RunShared`；这里不再直接用。
        resolve_workflow: _,
    } = input;

    let (_rt, ctx) = setup_workflow_runtime(&cancel).await?;

    let notify = Arc::new(Notify::new());
    let dispatch_q: Arc<Mutex<VecDeque<Dispatch>>> = Arc::new(Mutex::new(VecDeque::new()));

    let control = Control::new();
    publish_control_plane(&control, control_sink.as_ref());

    let agent_count = shared.count.clone();
    let (settle_tx, mut settle_rx) = unbounded_channel::<Settle>();
    let (done_tx, mut done_rx) = unbounded_channel::<String>();
    let args_json = args.as_ref().map(|v| v.to_string());

    if let Some(json) = &args_json
        && json.len() > MAX_BOUNDARY_BYTES
    {
        return Err(format!(
            "workflow args serialize to {} bytes, over the limit of {MAX_BOUNDARY_BYTES}.",
            json.len()
        ));
    }

    // 带 `resumed` 标记的 journal 整盘不重放（先付全价，而不是卡在第一个 resume 上）
    let mut journal = journal;
    let prefix_intact = journal.as_mut().is_some_and(|j| j.replayable());

    ctx.async_with(async move |ctx| -> Result<WorkflowOutcome, String> {
        let mut prefix_intact = prefix_intact;

        install_host_globals(
            &ctx,
            &HostBridge {
                dispatch_q: dispatch_q.clone(),
                notify: notify.clone(),
                progress: progress.clone(),
                spent: spent.clone(),
                phase: shared.phase.clone(),
                done_tx,
                args_json,
                limits,
            },
        )?;

        eval_workflow_script(&ctx, &body)?;

        let sem = Arc::new(Semaphore::new(limits.max_concurrency.max(1)));

        let outcome: Result<WorkflowOutcome, String> = loop {
            if cancel.load(Ordering::Relaxed) {
                // 掐 run = 掐它的全部子代理（每个活跃调用一个标志）
                for call in control.live.lock().unwrap().values() {
                    call.abort.store(true, Ordering::Relaxed);
                    call.wake.notify_waiters();
                }
                break Err("workflow aborted".to_string());
            }

            tokio::select! {
                done = done_rx.recv() => { break finish_outcome(done, &agent_count); }
                Some(s) = settle_rx.recv() => {
                    // 同步调用 JS `__settle`，随即丢弃该 Function（不跨 await）
                    let settle: Function = ctx
                        .globals()
                        .get("__settle")
                        .map_err(|e| format!("get __settle: {e}"))?;
                    settle
                        .call::<_, ()>((s.id, s.ok, s.json))
                        .map_err(|e| format!("__settle failed: {e}"))?;
                }
                _ = notify.notified() => {}
                _ = tokio::time::sleep(TICK) => {}
            }

            // 把这一轮 JS 产生的调用派发出去（spawn 到当前运行时）
            drain_dispatches(
                &shared,
                &dispatch_q,
                &settle_tx,
                &runner,
                &cancel,
                &spent,
                &progress,
                &schema,
                limits,
                &mut journal,
                &mut prefix_intact,
                &sem,
                &control,
                &agent_count,
            )
            .await;
        };

        outcome
    })
    .await
}

/// 建一个带"取消/墙钟超时"中断处理的 JS 运行时与上下文。
///
/// 返回 `rt` 一并持有：上下文内部虽已 clone 运行时，但显式保留它让中断处理器活到运行结束。
async fn setup_workflow_runtime(
    cancel: &Arc<AtomicBool>,
) -> Result<(AsyncRuntime, AsyncContext), String> {
    let rt = AsyncRuntime::new().map_err(|e| format!("workflow runtime: {e}"))?;
    let cancel_h = cancel.clone();
    let start = Instant::now();

    rt.set_interrupt_handler(Some(Box::new(move || {
        cancel_h.load(Ordering::Relaxed) || start.elapsed() > Duration::from_millis(MAX_WALL_MS)
    })))
    .await;

    let ctx = AsyncContext::full(&rt)
        .await
        .map_err(|e| format!("workflow context: {e}"))?;
    Ok((rt, ctx))
}

/// 把控制面（暂停/继续/跳过/重跑）交给调用方；没有 sink 就只留内部使用。
fn publish_control_plane(
    control: &Arc<Control>,
    control_sink: Option<&Arc<dyn Fn(WorkflowControl) + Send + Sync>>,
) {
    let Some(sink) = control_sink else {
        return;
    };

    let ctl = control.clone();
    let handle = WorkflowControl {
        pause: Arc::new({
            let ctl = ctl.clone();
            move || ctl.paused.store(true, Ordering::Relaxed)
        }),
        resume: Arc::new({
            let ctl = ctl.clone();
            move || {
                ctl.paused.store(false, Ordering::Relaxed);
                ctl.wake_all();
            }
        }),
        is_paused: Arc::new({
            let ctl = ctl.clone();
            move || ctl.paused.load(Ordering::Relaxed)
        }),
        skip: Arc::new({
            let ctl = ctl.clone();
            move |index| mark_intent(&ctl, index, Intent::Skip)
        }),
        retry: Arc::new({
            let ctl = ctl.clone();
            move |index| mark_intent(&ctl, index, Intent::Retry)
        }),
    };

    sink(handle);
}

/// 注入宿主函数与输入时用到的运行时句柄。
struct HostBridge {
    /// `__dispatch` 写入的调用队列。
    dispatch_q: Arc<Mutex<VecDeque<Dispatch>>>,
    /// 入队后唤醒驱动循环。
    notify: Arc<Notify>,
    /// `__progress` 追加的共享进度日志。
    progress: ProgressLog,
    /// `__budgetSpent` 读的 output token 累计。
    spent: Arc<AtomicU64>,
    /// `__allocPhase` 的阶段序号分配器。
    phase: Arc<AtomicU64>,
    /// `__done` 交回结局的通道。
    done_tx: UnboundedSender<String>,
    /// `__argsJson` 只读全局。
    args_json: Option<String>,
    /// `__itemCap` 等上限的来源。
    limits: WorkflowLimits,
}

/// 注入 `__dispatch` / `__progress` / `__budgetSpent` / `__allocPhase` / `__done` 与几个只读输入全局。
fn install_host_globals(ctx: &Ctx<'_>, bridge: &HostBridge) -> Result<(), String> {
    let q = bridge.dispatch_q.clone();
    let n = bridge.notify.clone();

    let dispatch = Function::new(ctx.clone(), move |id: u64, method: String, json: String| {
        q.lock().unwrap().push_back(Dispatch { id, method, json });
        n.notify_one();
    })
    .map_err(|e| format!("inject __dispatch: {e}"))?;
    ctx.globals()
        .set("__dispatch", dispatch)
        .map_err(|e| format!("set __dispatch: {e}"))?;

    let p = bridge.progress.clone();
    let progress_fn = Function::new(ctx.clone(), move |json: String| {
        if let Ok(v) = serde_json::from_str::<Value>(&json) {
            p.lock().unwrap().push(v);
        }
    })
    .map_err(|e| format!("inject __progress: {e}"))?;
    ctx.globals()
        .set("__progress", progress_fn)
        .map_err(|e| format!("set __progress: {e}"))?;

    let sp = bridge.spent.clone();
    let budget_fn = Function::new(ctx.clone(), move || sp.load(Ordering::Relaxed) as f64)
        .map_err(|e| format!("inject __budgetSpent: {e}"))?;
    ctx.globals()
        .set("__budgetSpent", budget_fn)
        .map_err(|e| format!("set __budgetSpent: {e}"))?;

    // 阶段序号由宿主分配：嵌套脚本共享同一个计数器，两张阶段树不会撞索引。
    let pc = bridge.phase.clone();
    let alloc = Function::new(ctx.clone(), move || {
        pc.fetch_add(1, Ordering::Relaxed) as i64
    })
    .map_err(|e| format!("inject __allocPhase: {e}"))?;
    ctx.globals()
        .set("__allocPhase", alloc)
        .map_err(|e| format!("set __allocPhase: {e}"))?;

    let done_tx = bridge.done_tx.clone();
    let done = Function::new(ctx.clone(), move |json: String| {
        let _ = done_tx.send(json);
    })
    .map_err(|e| format!("inject __done: {e}"))?;
    ctx.globals()
        .set("__done", done)
        .map_err(|e| format!("set __done: {e}"))?;

    ctx.globals()
        .set("__argsJson", bridge.args_json.clone())
        .map_err(|e| format!("set __argsJson: {e}"))?;
    ctx.globals()
        .set("__itemCap", bridge.limits.max_items as i64)
        .map_err(|e| format!("set __itemCap: {e}"))?;
    ctx.globals()
        .set("__boundaryCap", MAX_BOUNDARY_BYTES as i64)
        .map_err(|e| format!("set __boundaryCap: {e}"))?;
    Ok(())
}

/// 确定性前奏 + JS glue + 脚本体依次求值。
fn eval_workflow_script(ctx: &Ctx<'_>, body: &str) -> Result<(), String> {
    ctx.eval::<(), _>(DETERMINISM_PRELUDE.as_bytes())
        .map_err(|e| format!("determinism prelude failed: {e}"))?;
    ctx.eval::<(), _>(GLUE)
        .map_err(|e| format!("workflow glue failed: {e}"))?;
    ctx.eval::<(), _>(wrap_body(body).into_bytes())
        .map_err(|e| format!("workflow script failed to start: {e}"))?;
    Ok(())
}

/// `__done` 载荷 → 运行结局；缺 `ok:true` 一律算失败。
fn finish_outcome(
    done: Option<String>,
    agent_count: &Arc<AtomicU64>,
) -> Result<WorkflowOutcome, String> {
    let payload = done.unwrap_or_else(|| {
        "{\"ok\":false,\"message\":\"workflow ended without a result\"}".to_string()
    });

    let parsed: Value = serde_json::from_str(&payload).unwrap_or(
        serde_json::json!({ "ok": false, "message": "workflow returned malformed result" }),
    );

    let ok = parsed.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
    if ok {
        Ok(WorkflowOutcome {
            value: parsed.get("value").cloned().unwrap_or(Value::Null),
            agent_count: agent_count.load(Ordering::Relaxed),
        })
    } else {
        Err(parsed
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("workflow failed")
            .to_string())
    }
}

/// 把这一轮 JS 产生的调用全部处理掉：`workflow()` 内联跑，`agent()` spawn 到当前运行时。
#[allow(clippy::too_many_arguments)]
async fn drain_dispatches(
    shared: &Arc<RunShared>,
    dispatch_q: &Arc<Mutex<VecDeque<Dispatch>>>,
    settle_tx: &UnboundedSender<Settle>,
    runner: &AgentRunner,
    cancel: &Arc<AtomicBool>,
    spent: &Arc<AtomicU64>,
    progress: &ProgressLog,
    schema: &Option<SchemaHooks>,
    limits: WorkflowLimits,
    journal: &mut Option<Journal>,
    prefix_intact: &mut bool,
    sem: &Arc<Semaphore>,
    control: &Arc<Control>,
    agent_count: &Arc<AtomicU64>,
) {
    loop {
        let next = dispatch_q.lock().unwrap().pop_front();
        let Some(d) = next else { break };

        if d.method == "workflow" {
            let res = run_nested(
                shared, &d.json, runner, cancel, spent, progress, schema, limits,
            )
            .await;

            let (ok, json) = match res {
                Ok(v) => (
                    true,
                    serde_json::to_string(&v).unwrap_or_else(|_| "null".into()),
                ),
                Err((message, fatal)) => (false, settle_payload(&message, Some(fatal))),
            };

            _ = settle_tx.send(Settle { id: d.id, ok, json });
            continue;
        }

        if d.method != "agent" {
            settle_error(
                settle_tx,
                d.id,
                format!("unsupported workflow host method: {}", d.method),
                None,
            );
            continue;
        }

        let req = match parse_agent_request(&d.json) {
            Ok(r) => r,
            Err(e) => {
                settle_error(settle_tx, d.id, e, None);
                continue;
            }
        };

        // schema 闸门：在 `agent_count` 与并发 permit **之前**，
        // 所以失败的调用既不占并发额度也不计入配额。它是**运行级**致命错误
        // （schema 写错了，重试同一个 agent 毫无意义），故这里 settle `fatal`。
        if let Some(hooks) = schema
            && let Some(sch) = req.schema.as_ref()
            && let Err(message) = (hooks.compile)(sch)
        {
            settle_error(settle_tx, d.id, message, Some(true));
            continue;
        }

        // 配额：先查后加，被拒的调用不消耗计数器。
        if agent_count.load(Ordering::Relaxed) >= limits.max_agents {
            settle_error(
                settle_tx,
                d.id,
                format!("workflow exceeded its agent limit of {}", limits.max_agents),
                Some(true),
            );
            continue;
        }

        // `index` 与计数器是同一个数：它同时是进度条目的身份、`wf-agent-N` 句柄与 journal 的位置。
        let index = agent_count.fetch_add(1, Ordering::Relaxed);
        let mut entry = progress_entry_base(index, &req);
        push_progress(progress, entry.clone());

        let journal_path = journal.as_ref().and_then(|j| j.path.clone());
        let journal_key = journal_key_for(&req);
        if let Some(text) = try_replay(journal, prefix_intact, index, &journal_key, &req, schema) {
            settle_replayed(
                progress,
                &mut entry,
                journal_path.as_deref(),
                index,
                &journal_key,
                &text,
                settle_tx,
                d.id,
                &req,
            );
            continue;
        }

        // 登记这一格：控制面（skip/retry）与"掐 run 就掐它的子代理"都靠它。
        let abort = Arc::new(AtomicBool::new(false));
        let wake = Arc::new(Notify::new());
        control.live.lock().unwrap().insert(
            index,
            LiveCall {
                abort: abort.clone(),
                wake: wake.clone(),
                started: false,
                settled: false,
                intent: None,
            },
        );

        let resumed_call = req.resume.is_some();
        tokio::spawn(run_agent_call(
            control.clone(),
            index,
            cancel.clone(),
            sem.clone(),
            progress.clone(),
            spent.clone(),
            settle_tx.clone(),
            journal_path,
            journal_key,
            d.id,
            resumed_call,
            entry,
            runner.clone(),
            req,
            wake,
        ));
    }
}

/// 失败载荷：`fatal` 只在明确给了值时写（保留"缺省 vs false"的区分）。
fn settle_payload(message: &str, fatal: Option<bool>) -> String {
    let mut payload = serde_json::json!({ "message": message });
    if let Some(fatal) = fatal {
        payload["fatal"] = serde_json::json!(fatal);
    }
    payload.to_string()
}

/// 结算一条失败的 `agent()` 调用（脚本侧看到 null 或 fatal）。
fn settle_error(tx: &UnboundedSender<Settle>, id: u64, message: String, fatal: Option<bool>) {
    _ = tx.send(Settle {
        id,
        ok: false,
        json: settle_payload(&message, fatal),
    });
}

/// 一次调用的 journal 键（改了 prompt/model/schema 就不再命中重放）。
fn journal_key_for(req: &AgentRequest) -> String {
    journal::key(&JournalKeyInput {
        prompt: req.prompt.clone(),
        label: req.label.clone(),
        model: req.model.clone(),
        agent_type: req.agent_type.clone(),
        effort: req.effort.clone(),
        gate: req.gate.clone(),
        resume: req.resume.clone(),
        schema: req
            .schema
            .as_ref()
            .map(|s| serde_json::to_string(s).unwrap_or_default()),
    })
}

/// 尝试从 journal 重放第 `index` 次调用；命中返回文本。
///
/// 任何一处对不上就**永久**结束本次运行的可重放前缀（`*prefix_intact = false`）：
/// 乱序复用后面的命中等于复用在不同上游条件下产出的结果。
fn try_replay(
    journal: &Option<Journal>,
    prefix_intact: &mut bool,
    index: u64,
    journal_key: &str,
    req: &AgentRequest,
    schema: &Option<SchemaHooks>,
) -> Option<String> {
    if !*prefix_intact {
        return None;
    }

    match journal.as_ref().and_then(|j| j.entries.get(index as usize)) {
        Some(e) if e.index == index && e.key == journal_key && e.ok => {
            let text = e.text.clone().unwrap_or_default();
            match (schema.as_ref(), req.schema.as_ref()) {
                // 重放的文本仍要过一遍 schema：journal 可能被手改过
                (Some(hooks), Some(sch)) => {
                    if (hooks.check_text)(sch, &text).is_ok() {
                        Some(text)
                    } else {
                        *prefix_intact = false;
                        None
                    }
                }
                _ => Some(text),
            }
        }
        _ => {
            *prefix_intact = false;
            None
        }
    }
}

/// 重放命中：写 progress、补记 journal，并把结果交回脚本。
#[allow(clippy::too_many_arguments)]
fn settle_replayed(
    progress: &ProgressLog,
    entry: &mut Value,
    journal_path: Option<&Path>,
    index: u64,
    journal_key: &str,
    text: &str,
    tx: &UnboundedSender<Settle>,
    settle_id: u64,
    req: &AgentRequest,
) {
    let now = now_ms();
    entry["state"] = serde_json::json!(EntryState::Done.as_str());
    entry["cached"] = serde_json::json!(true);
    entry["startedAt"] = serde_json::json!(now);
    entry["lastProgressAt"] = serde_json::json!(now);
    entry["durationMs"] = serde_json::json!(0);
    entry["resultPreview"] = serde_json::json!(preview(text));
    push_progress(progress, entry.clone());

    // 也记进**本次**运行的 journal：恢复的恢复不必再回走一条链
    record_journal(
        journal_path,
        &JournalEntry {
            index,
            key: journal_key.to_string(),
            ok: true,
            text: Some(text.to_string()),
            resumed: false,
        },
    );

    // 有 schema 的调用：journal 存的是那段 JSON（脚本侧会 parse 成对象）；
    // 无 schema 的调用存的是纯文本，交回前要按字符串编码。
    let json = if req.schema.is_some() {
        text.to_string()
    } else {
        serde_json::to_string(text).unwrap_or_else(|_| "null".into())
    };

    _ = tx.send(Settle {
        id: settle_id,
        ok: true,
        json,
    });
}

/// 一次 `agent()` 调用的执行体：暂停/跳过 → 拿并发槽 → 跑（可重试）→ 记账 → settle。
#[allow(clippy::too_many_arguments)]
async fn run_agent_call(
    control: Arc<Control>,
    index: u64,
    cancel: Arc<AtomicBool>,
    sem: Arc<Semaphore>,
    progress: ProgressLog,
    spent: Arc<AtomicU64>,
    tx: UnboundedSender<Settle>,
    journal_path: Option<PathBuf>,
    journal_key: String,
    id: u64,
    resumed_call: bool,
    mut entry: Value,
    runner: AgentRunner,
    mut req: AgentRequest,
    wake: Arc<Notify>,
) {
    // 暂停：只拦"启动"，已在跑的不动
    while control.paused.load(Ordering::Relaxed) {
        if take_intent(&control, index) == Some(Intent::Skip) || cancel.load(Ordering::Relaxed) {
            break;
        }
        tokio::select! {
            _ = wake.notified() => {}
            _ = tokio::time::sleep(TICK) => {}
        }
    }

    if take_intent(&control, index) == Some(Intent::Skip) {
        settle_skipped(
            &progress,
            &mut entry,
            index,
            &journal_path,
            &journal_key,
            &tx,
            id,
        );
        mark_settled(&control, index);
        return;
    }

    let _permit = sem.acquire_owned().await;
    mark_started(&control, index);

    let started_ms = now_ms(); // 拿到槽位才算"已开始"：在此之前行是 queued。

    // 各次跃迁**改同一个 Value 再整体重发**：折叠按 index last-write-wins，所以结算那条必须带着前面累积的字段。
    entry["state"] = serde_json::json!(EntryState::Progress.as_str());
    entry["startedAt"] = serde_json::json!(started_ms);
    entry["lastProgressAt"] = serde_json::json!(started_ms);
    push_progress(&progress, entry.clone());

    // retry 重跑**同一次调用**，脚本那条 promise 还在等
    let (report, _retried) = loop {
        req.abort = control
            .live
            .lock()
            .unwrap()
            .get(&index)
            .map(|c| c.abort.clone())
            .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));

        // id 回传点接**当前** entry 快照：runner 一拿到记录 id 就写进进度日志。
        // 这样运行中的行也有 `recordId`，检查器 `c` 在 workflow 没结束前就能打开会话。
        req.on_spawn = SpawnHook::new({
            let progress = progress.clone();
            let base = entry.clone();
            move |record_id: String| {
                let mut e = base.clone();
                e["recordId"] = serde_json::json!(record_id);
                e["lastProgressAt"] = serde_json::json!(now_ms());
                push_progress(&progress, e);
            }
        });

        let report = runner(req.clone()).await;

        match take_intent(&control, index) {
            Some(Intent::Retry) => {
                // 停掉的那个 child 已经让出位置；换一个新标志再跑一次
                let fresh = Arc::new(AtomicBool::new(false));
                if let Some(call) = control.live.lock().unwrap().get_mut(&index) {
                    call.abort = fresh;
                }

                let attempt = entry["attempt"].as_u64().unwrap_or(1) + 1;
                entry["attempt"] = serde_json::json!(attempt);
                entry["lastAttemptReason"] = serde_json::json!("user-retry");
                entry["state"] = serde_json::json!(EntryState::Progress.as_str());
                entry["lastProgressAt"] = serde_json::json!(now_ms());
                push_progress(&progress, entry.clone());

                if cancel.load(Ordering::Relaxed) {
                    break (report, true);
                }

                continue;
            }
            Some(Intent::Skip) => {
                settle_skipped(
                    &progress,
                    &mut entry,
                    index,
                    &journal_path,
                    &journal_key,
                    &tx,
                    id,
                );
                mark_settled(&control, index);
                return;
            }
            None => break (report, false),
        }
    };

    // `journal_text` 是**原文**（文本结果就是文本，结构化结果是那段 JSON），
    // 与交给脚本的 `json` 编码不同：journal 存原文，settle 走 JSON 边界。
    let (ok, json, journal_text, error) = match report.result {
        Ok(AgentOutcome::Text(t)) => (
            true,
            serde_json::to_string(&t).unwrap_or_else(|_| "null".into()),
            Some(t),
            None,
        ),
        Ok(AgentOutcome::Structured(v)) => {
            let text = v.to_string();
            (true, text.clone(), Some(text), None)
        }
        // 普通失败 → `null`（不是 reject）：脚本的 `.filter(Boolean)` 就是为此写的。
        // 原因不丢：它落在进度条目的 `error` 上，由覆盖层与完成卡片展示。
        Err(e) => (true, "null".to_string(), None, Some(e)),
    };

    let settled_state = if error.is_some() {
        EntryState::Error
    } else {
        EntryState::Done
    };

    entry["state"] = serde_json::json!(settled_state.as_str());
    entry["lastProgressAt"] = serde_json::json!(now_ms());
    entry["durationMs"] = serde_json::json!(
        report
            .meta
            .duration_ms
            .unwrap_or_else(|| now_ms().saturating_sub(started_ms))
    );
    apply_meta(&mut entry, &report.meta);

    if let Some(out) = report.meta.output_tokens {
        spent.fetch_add(out, Ordering::Relaxed);
    }

    let failed = error.is_some();
    if let Some(err) = error {
        entry["error"] = serde_json::json!(err);
    }

    push_progress(&progress, entry);

    // 结算即落 journal：被中途掐掉的运行也留下它已经跑完的那些
    record_journal(
        journal_path.as_deref(),
        &super::journal::JournalEntry {
            index,
            key: journal_key,
            ok: !failed,
            text: if failed { None } else { journal_text },
            resumed: resumed_call,
        },
    );

    mark_settled(&control, index);

    _ = tx.send(Settle { id, ok, json });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runner_echo() -> AgentRunner {
        Arc::new(|req: AgentRequest| {
            Box::pin(async move {
                if req.schema.is_some() {
                    // 有 schema 时返回结构化结果（模拟 StructuredOutput）
                    AgentReport::structured(serde_json::json!({ "echo": req.prompt }))
                } else {
                    AgentReport::text(format!("echo:{}", req.prompt))
                }
            })
        })
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    /// 新的共享进度日志（run 持有；kernel 只追加）。
    fn log() -> super::super::task::ProgressLog {
        Arc::new(Mutex::new(Vec::new()))
    }

    fn run(body: &str, args: Option<Value>) -> Result<(WorkflowOutcome, Vec<Value>), String> {
        run_with_log(
            body,
            args,
            runner_echo(),
            log(),
            WorkflowLimits::default(),
            None,
        )
    }

    fn run_with_log(
        body: &str,
        args: Option<Value>,
        runner: AgentRunner,
        progress: super::super::task::ProgressLog,
        limits: WorkflowLimits,
        schema: Option<SchemaHooks>,
    ) -> Result<(WorkflowOutcome, Vec<Value>), String> {
        let cancel = Arc::new(AtomicBool::new(false));
        let spent = Arc::new(AtomicU64::new(0));
        let out = rt().block_on(run_script(WorkflowInputs {
            body: body.to_string(),
            args,
            limits,
            runner,
            cancel,
            spent,
            progress: progress.clone(),
            schema,
            journal: None,
            control_sink: None,
            resolve_workflow: None,
        }))?;
        let entries = progress.lock().unwrap().clone();
        Ok((out, entries))
    }

    #[test]
    fn agent_parallel_pipeline_phase_log_args_round_trip() {
        let body = r#"
log('auditing ' + args.files.length + ' files')
phase('Scan')
const scanned = await parallel(args.files.map(f => () => agent('scan ' + f)))
phase('Fix')
const fixed = await pipeline(scanned, (s) => agent('fix ' + s), (r, item) => agent('verify ' + item))
return { scanned, fixed, spent: budget.spent() }
"#;
        let (out, entries) =
            run(body, Some(serde_json::json!({ "files": ["a.ts", "b.ts"] }))).unwrap();
        assert_eq!(
            out.value["scanned"],
            serde_json::json!(["echo:scan a.ts", "echo:scan b.ts"])
        );
        assert_eq!(
            out.value["fixed"][0],
            serde_json::json!("echo:verify echo:scan a.ts")
        );
        // 6 个 agent：2 scan + 2 fix + 2 verify
        assert_eq!(out.agent_count, 6);
        // 两条 phase + 一条 log + 六个 agent 条目（每个三条：start/progress/done）
        let kinds: Vec<&str> = entries
            .iter()
            .filter_map(|p| p.get("type").and_then(|v| v.as_str()))
            .collect();
        assert!(kinds.contains(&"workflow_phase"));
        assert!(kinds.contains(&"workflow_log"));
        let agent_entries = kinds.iter().filter(|k| **k == "workflow_agent").count();
        assert_eq!(agent_entries, 6 * 3, "每个 agent 三条跃迁：{entries:?}");
        // index 与 `wf-agent-N` 句柄成对
        assert!(
            entries
                .iter()
                .any(|e| e["index"] == 0 && e["agentId"] == "wf-agent-0")
        );
        // 结算条目带上 meta 与用时
        let settled: Vec<&Value> = entries
            .iter()
            .filter(|e| e["type"] == "workflow_agent" && e["state"] == "done")
            .collect();
        assert_eq!(settled.len(), 6);
        assert!(
            settled
                .iter()
                .all(|e| e["startedAt"].is_u64() && e["durationMs"].is_u64())
        );
    }

    /// 嵌套 `workflow()`：宿主解析器找到子脚本，子脚本代理共享配额/阶段序号，
    /// 返回值交回外层。
    #[test]
    fn nested_workflow_runs_and_returns_its_value() {
        let resolver: Arc<ResolveWorkflowFn> = Arc::new(|payload: &Value| {
            let name = payload.get("name").and_then(|v| v.as_str()).unwrap_or("");
            if name != "child" {
                return Err(format!("unknown workflow {name:?}"));
            }
            Ok(ResolvedWorkflow {
                name: "child".to_string(),
                body: "const r = await agent('child-a'); return { got: r }".to_string(),
            })
        });
        let out = rt()
            .block_on(run_script(WorkflowInputs {
                body: "const v = await workflow('child'); return v".to_string(),
                args: None,
                limits: WorkflowLimits::default(),
                runner: runner_echo(),
                cancel: Arc::new(AtomicBool::new(false)),
                spent: Arc::new(AtomicU64::new(0)),
                progress: log(),
                schema: None,
                journal: None,
                control_sink: None,
                resolve_workflow: Some(resolver),
            }))
            .unwrap();
        assert_eq!(out.value, serde_json::json!({ "got": "echo:child-a" }));
        assert_eq!(out.agent_count, 1, "子脚本的代理计入同一计数器");
    }

    /// 嵌套只允许一层；未知名字是脚本可捕获的错误。
    #[test]
    fn nested_workflow_enforces_one_level_and_surfaces_unknown_names() {
        let resolver: Arc<ResolveWorkflowFn> = Arc::new(|payload: &Value| {
            let name = payload.get("name").and_then(|v| v.as_str()).unwrap_or("");
            if name == "child" {
                Ok(ResolvedWorkflow {
                    name: "child".to_string(),
                    body: "return await workflow('grandchild')".to_string(),
                })
            } else {
                Err(format!("unknown workflow {name:?}"))
            }
        });
        let err = rt()
            .block_on(run_script(WorkflowInputs {
                body: "return await workflow('child')".to_string(),
                args: None,
                limits: WorkflowLimits::default(),
                runner: runner_echo(),
                cancel: Arc::new(AtomicBool::new(false)),
                spent: Arc::new(AtomicU64::new(0)),
                progress: log(),
                schema: None,
                journal: None,
                control_sink: None,
                resolve_workflow: Some(resolver),
            }))
            .unwrap_err();
        assert!(err.contains("one level deep"), "{err}");

        // 未知名字：glue 的属性化 error 可被脚本捕获
        let out = rt()
            .block_on(run_script(WorkflowInputs {
                body: "try { await workflow('nope') } catch (e) { return 'caught: ' + e.message }\nreturn 'no'".to_string(),
                args: None,
                limits: WorkflowLimits::default(),
                runner: runner_echo(),
                cancel: Arc::new(AtomicBool::new(false)),
                spent: Arc::new(AtomicU64::new(0)),
                progress: log(),
                schema: None,
                journal: None,
                control_sink: None,
                resolve_workflow: Some(Arc::new(|_p: &Value| Err("unknown workflow".to_string()))),
            }))
            .unwrap();
        assert!(
            out.value.as_str().unwrap_or("").starts_with("caught:"),
            "{:?}",
            out.value
        );
    }

    #[test]
    fn dropped_agent_launch_fails_the_run() {
        let err = run("agent('fire and forget'); return 1", None).unwrap_err();
        assert!(err.contains("unawaited agent launch"), "{err}");
        assert!(err.contains("fire and forget"), "{err}");
    }

    #[test]
    fn awaited_and_returned_launches_are_not_flagged() {
        // `return agent(...)` 与 `await agent(...)` 都算已认领。
        run("return agent('a')", None).unwrap();
        run("await agent('b'); return 1", None).unwrap();
    }

    #[test]
    fn agent_isolation_option_is_parsed_and_forwarded() {
        let seen: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));
        let seen2 = seen.clone();
        let runner: AgentRunner = Arc::new(move |req: AgentRequest| {
            seen2.lock().unwrap().push(req.isolation.clone());
            Box::pin(async move { AgentReport::text("ok") })
        });
        run_with_log(
            "await agent('a', { isolation: 'worktree' }); return 1",
            None,
            runner,
            log(),
            WorkflowLimits::default(),
            None,
        )
        .unwrap();
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            &[Some("worktree".to_string())]
        );
    }

    #[test]
    fn agent_isolation_rejects_non_worktree_values() {
        let err = run("await agent('a', { isolation: 'remote' })", None).unwrap_err();
        assert!(err.contains("isolation must be"), "{err}");
    }

    /// runner 拿到记录 id 就通过 `on_spawn` 回传，桥**立即**写进进度条目——
    /// 不必等结算，所以检查器能在 workflow 还在跑时就用 `recordId` 打开会话。
    #[test]
    fn spawn_hook_writes_record_id_before_the_agent_settles() {
        let progress = log();
        let progress_for_runner = progress.clone();
        let seen_mid_run: Arc<Mutex<Vec<bool>>> = Arc::new(Mutex::new(Vec::new()));
        let seen = seen_mid_run.clone();

        let runner: AgentRunner = Arc::new(move |req: AgentRequest| {
            let progress = progress_for_runner.clone();
            let seen = seen.clone();
            Box::pin(async move {
                if let Some(cb) = req.on_spawn.callback() {
                    cb("rec-mid-run".to_string());
                }
                // 还在 runner 里面（未结算）：进度日志应已能看到 recordId
                let has = progress
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|e| e["type"] == "workflow_agent" && e["recordId"] == "rec-mid-run");
                seen.lock().unwrap().push(has);
                AgentReport::text("ok")
            })
        });

        let (out, entries) = run_with_log(
            "await agent('a'); await agent('b'); return 1",
            None,
            runner,
            progress,
            WorkflowLimits::default(),
            None,
        )
        .unwrap();

        assert_eq!(out.value, serde_json::json!(1));
        assert_eq!(
            seen_mid_run.lock().unwrap().as_slice(),
            &[true, true],
            "回传必须发生在结算之前"
        );
        assert!(
            entries.iter().any(|e| e["recordId"] == "rec-mid-run"),
            "{entries:?}"
        );
    }

    #[test]
    fn schema_agent_returns_object() {
        let body = r#"
const r = await agent('find bugs', { schema: { type: 'object' } })
return r
"#;
        let (out, _) = run(body, None).unwrap();
        assert_eq!(out.value, serde_json::json!({ "echo": "find bugs" }));
    }

    #[test]
    fn determinism_prelude_blocks_clock_and_random() {
        // Date.now() 应抛错 → __done(ok:false)
        let err = run("return Date.now()", None).unwrap_err();
        assert!(err.contains("Date.now"), "{err}");
        let err = run("return Math.random()", None).unwrap_err();
        assert!(err.contains("Math.random"), "{err}");
        // 代码生成禁用：直接 `eval` / `new Function` 也抛
        let err = run("return eval('1 + 1')", None).unwrap_err();
        assert!(err.contains("eval()"), "{err}");
        let err = run("return new Function('return 1')()", None).unwrap_err();
        assert!(err.contains("Function"), "{err}");
    }

    #[test]
    fn parallel_swallows_stage_error_to_null() {
        let runner: AgentRunner = Arc::new(|req: AgentRequest| {
            Box::pin(async move {
                if req.prompt.contains("boom") {
                    AgentReport::failed("kaboom")
                } else {
                    AgentReport::text(req.prompt)
                }
            })
        });
        let (out, entries) = run_with_log(
            "const r = await parallel([() => agent('ok'), () => agent('boom')]); return r",
            None,
            runner,
            log(),
            WorkflowLimits::default(),
            None,
        )
        .unwrap();
        assert_eq!(out.value, serde_json::json!(["ok", null]));
        // 失败原因落在该条目的 `error` 上（不是丢掉）
        assert!(
            entries.iter().any(|e| e["type"] == "workflow_agent"
                && e["state"] == "error"
                && e["error"] == "kaboom"),
            "{entries:?}"
        );
    }

    type ControlSink = Arc<dyn Fn(WorkflowControl) + Send + Sync>;
    /// 控制面暂存槽（测试用）。
    type ControlSlot = Arc<std::sync::Mutex<Option<WorkflowControl>>>;

    /// 收集控制面：kernel 在第一个 agent 开跑前把 `WorkflowControl` 交回，存起来给测试用。
    fn control_slot() -> (ControlSlot, ControlSink) {
        let slot: ControlSlot = Arc::new(std::sync::Mutex::new(None));
        let sink_slot = slot.clone();
        (
            slot,
            Arc::new(move |ctl: WorkflowControl| {
                *sink_slot.lock().unwrap() = Some(ctl);
            }),
        )
    }

    /// 跑一个带控制面的运行。
    fn run_with_control(
        body: &str,
        runner: AgentRunner,
        progress: super::super::task::ProgressLog,
        journal: Option<super::super::journal::Journal>,
        sink: ControlSink,
    ) -> Result<(WorkflowOutcome, Vec<Value>), String> {
        let out = rt().block_on(run_script(WorkflowInputs {
            body: body.to_string(),
            args: None,
            limits: WorkflowLimits::default(),
            runner,
            cancel: Arc::new(AtomicBool::new(false)),
            spent: Arc::new(AtomicU64::new(0)),
            progress: progress.clone(),
            schema: None,
            journal,
            control_sink: Some(sink),
            resolve_workflow: None,
        }))?;
        Ok((out, progress.lock().unwrap().clone()))
    }

    /// 暂停只拦"启动"：第一个 agent 跑完后暂停，第二个不再开跑；`resume()` 放行。
    #[test]
    fn pause_holds_new_agents_until_resume() {
        let progress = log();
        let (slot, sink) = control_slot();
        let runner_slot = slot.clone();
        let runner: AgentRunner = Arc::new(move |req: AgentRequest| {
            let slot = runner_slot.clone();
            Box::pin(async move {
                // 第一个 agent 结算前把整次运行暂停（模拟用户在检查器里按 p）
                if req.prompt.contains("one")
                    && let Some(ctl) = slot.lock().unwrap().clone()
                {
                    ctl.pause();
                }
                AgentReport::text(format!("echo:{}", req.prompt))
            })
        });
        let body = "const a = await agent('one'); const b = await agent('two'); return { a, b }";
        let cancel = Arc::new(AtomicBool::new(false));
        // 同一个 runtime：`spawn` 出去的任务要靠随后的 `block_on` 驱动（current_thread）
        let rt = rt();
        let handle = rt.spawn(run_script(WorkflowInputs {
            body: body.to_string(),
            args: None,
            limits: WorkflowLimits::default(),
            runner,
            cancel,
            spent: Arc::new(AtomicU64::new(0)),
            progress: progress.clone(),
            schema: None,
            journal: None,
            control_sink: Some(sink),
            resolve_workflow: None,
        }));
        // 等第一个 agent 出现，然后确认第二个没有被启动、run 也没结束
        rt.block_on(async {
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            loop {
                let started = progress
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|e| e["type"] == "workflow_agent" && e["index"] == 0);
                if started {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "第一个 agent 没跑起来"
                );
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert!(!handle.is_finished(), "暂停后 run 不该自己结束");
            // 第二个 agent 的条目会出现（**queued**），但不该"已开始"（没有 startedAt）
            let started_second = progress.lock().unwrap().iter().any(|e| {
                e["type"] == "workflow_agent" && e["index"] == 1 && e["startedAt"].is_u64()
            });
            assert!(!started_second, "暂停期间不该启动第二个 agent");
            slot.lock().unwrap().as_ref().expect("control").resume();
            let out = handle.await.unwrap().unwrap();
            assert_eq!(
                out.value,
                serde_json::json!({ "a": "echo:one", "b": "echo:two" })
            );
        });
    }

    /// 跳过：脚本拿到 `null`，进度行标 skipped 并写明"用户跳过"，journal 记成失败
    /// （否则下次恢复会把它当成"上次成功"重放）。
    #[test]
    fn skip_settles_null_and_records_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        let journal_path = dir.path().join("wf.jsonl");
        let progress = log();
        let (slot, sink) = control_slot();
        let runner_slot = slot.clone();
        let runner: AgentRunner = Arc::new(move |req: AgentRequest| {
            let slot = runner_slot.clone();
            Box::pin(async move {
                // 进来就跳过自己（模拟用户在检查器里按 s）
                if let Some(ctl) = slot.lock().unwrap().clone() {
                    ctl.skip(0);
                }
                while !req.abort.load(Ordering::Relaxed) {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                AgentReport::text("unused")
            })
        });
        let (out, entries) = run_with_control(
            "const a = await agent('hold'); return { a }",
            runner,
            progress,
            Some(super::super::journal::Journal {
                entries: Vec::new(),
                path: Some(journal_path.clone()),
                resumed_from: None,
            }),
            sink,
        )
        .unwrap();
        assert_eq!(
            out.value,
            serde_json::json!({ "a": null }),
            "跳过的 agent 给 null"
        );
        let skipped = entries
            .iter()
            .find(|e| e["type"] == "workflow_agent" && e["state"] == "error")
            .expect("要有失败条目");
        assert_eq!(skipped["skipped"], true);
        assert_eq!(skipped["error"], "Skipped by user.");
        let journal = super::super::journal::read(&journal_path);
        assert_eq!(journal.len(), 1, "{journal:?}");
        assert!(!journal[0].ok, "跳过必须记成失败，否则下次会被重放");
    }

    /// 重跑：`retry` 只对正在跑的 agent 有效；它停掉当前 child、**重跑同一次调用**，
    /// 脚本那条 promise 拿到的仍是这一次调用的结果（新答案）。
    #[test]
    fn retry_reruns_the_same_call() {
        let progress = log();
        let (slot, sink) = control_slot();
        let attempts = Arc::new(AtomicU64::new(0));
        let runner_slot = slot.clone();
        let runner_attempts = attempts.clone();
        let runner: AgentRunner = Arc::new(move |req: AgentRequest| {
            let slot = runner_slot.clone();
            let n = runner_attempts.fetch_add(1, Ordering::SeqCst) + 1;
            Box::pin(async move {
                if n == 1 {
                    // 第一次进来就要求重跑（模拟用户在检查器里按 r）
                    if let Some(ctl) = slot.lock().unwrap().clone() {
                        assert!(ctl.retry(0), "正在跑的 agent 应可重跑");
                    }
                    while !req.abort.load(Ordering::Relaxed) {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    AgentReport::text("first attempt")
                } else {
                    AgentReport::text("second attempt")
                }
            })
        });
        let (out, entries) = run_with_control(
            "const a = await agent('x'); return { a }",
            runner,
            progress,
            None,
            sink,
        )
        .unwrap();
        assert_eq!(out.value, serde_json::json!({ "a": "second attempt" }));
        assert_eq!(attempts.load(Ordering::SeqCst), 2, "应当跑两次");
        let done = entries
            .iter()
            .find(|e| e["type"] == "workflow_agent" && e["state"] == "done")
            .expect("done 条目");
        assert_eq!(done["attempt"], 2);
        assert_eq!(done["lastAttemptReason"], "user-retry");
    }

    /// 结算之后的 skip/retry 是 no-op（脚本已经拿到值了）。
    #[test]
    fn control_after_settle_is_a_no_op() {
        let progress = log();
        let (slot, sink) = control_slot();
        let runner_slot = slot.clone();
        let runner: AgentRunner = Arc::new(move |_req: AgentRequest| {
            let _ = runner_slot.clone();
            Box::pin(async move { AgentReport::text("ok") })
        });
        let (out, _) = run_with_control(
            "return { a: await agent('x') }",
            runner,
            progress,
            None,
            sink,
        )
        .unwrap();
        assert_eq!(out.value, serde_json::json!({ "a": "ok" }));
        let ctl = slot.lock().unwrap().clone().expect("control");
        assert!(!ctl.skip(0), "结算后 skip 应为 no-op");
        assert!(!ctl.retry(0), "结算后 retry 应为 no-op");
    }

    #[test]
    fn agent_cap_is_fatal() {
        let err = run_with_log(
            "let n = 0; while (true) { await agent('x' + n); n++; } return n",
            None,
            runner_echo(),
            log(),
            WorkflowLimits {
                max_agents: 2,
                ..Default::default()
            },
            None,
        )
        .unwrap_err();
        assert!(err.contains("agent limit"), "{err}");
    }

    /// 普通 agent 失败 → `null`，不是抛（对齐上游 `respond(callId, true, null)`）。
    /// 顶层的 `await agent(...)` 也能观察到，脚本的 `.filter(Boolean)` 才有意义。
    #[test]
    fn ordinary_failure_resolves_to_null() {
        let runner: AgentRunner =
            Arc::new(|_req: AgentRequest| Box::pin(async move { AgentReport::failed("kaboom") }));
        let (out, entries) = run_with_log(
            "const r = await agent('x'); return { isNull: r === null, kind: typeof r }",
            None,
            runner,
            log(),
            WorkflowLimits::default(),
            None,
        )
        .unwrap();
        assert_eq!(
            out.value,
            serde_json::json!({ "isNull": true, "kind": "object" })
        );
        // 原因不丢：它落在进度条目的 `error` 上（覆盖层与完成卡片都读那里）
        assert!(
            entries.iter().any(|p| {
                p.get("type").and_then(|v| v.as_str()) == Some("workflow_agent")
                    && p.get("state").and_then(|v| v.as_str()) == Some("error")
                    && p.get("error").and_then(|v| v.as_str()) == Some("kaboom")
            }),
            "failure reason should reach the progress entry: {entries:?}"
        );
    }

    /// schema 闸门失败是**运行级**致命（不是该 agent 返回 null）。
    /// 同时验证它在 `agent_count` 之前跑：把配额设成 0，报的必须是闸门的消息，
    /// 而不是“超出 agent 上限”。
    #[test]
    fn schema_gate_failure_is_fatal_and_precedes_the_quota() {
        let gate: SchemaHooks = SchemaHooks::compile_only(Arc::new(|schema: &Value| {
            if schema.get("type").and_then(|v| v.as_str()) == Some("object") {
                Ok(())
            } else {
                Err("agent() opts.schema must have `type: \"object\"`".to_string())
            }
        }));
        let err = run_with_log(
            "return await agent('x', { schema: { type: 'string' } })",
            None,
            runner_echo(),
            log(),
            // 配额 0：若计数器先跑，错误会是 agent 上限
            WorkflowLimits {
                max_agents: 0,
                ..Default::default()
            },
            Some(gate),
        )
        .unwrap_err();
        assert!(err.contains("type: \"object\""), "{err}");
        assert!(!err.contains("agent limit"), "gate must run first: {err}");
    }

    /// JSON 边界：`JSON.stringify` 会静默降级的那些值必须被显式拒绝，而不是写进
    /// journal/progress 后变成对不上的形状。
    #[test]
    fn json_boundary_rejects_values_stringify_would_mangle() {
        let cases = [
            ("const a = {}; a.self = a; return a", "circular structure"),
            ("return { n: 0 / 0 }", "non-finite number"),
            ("return () => 1", "a function"),
            ("return new Map()", "non-plain object"),
            ("const a = []; a[2] = 1; return a", "sparse array"),
            (
                "return { n: { deep: [1, 2, { x: Infinity }] } }",
                "non-finite number",
            ),
        ];
        for (body, needle) in cases {
            let err = run(body, None).unwrap_err();
            assert!(err.contains(needle), "{body}: {err}");
            assert!(err.contains("workflow VM boundary"), "{body}: {err}");
        }
        // 普通 JSON 双向通过；顶层 `return undefined` 允许（归一为 null）
        run("return { a: [1, 'two', null, { b: true }] }", None).unwrap();
        let (out, _) = run("return undefined", None).unwrap();
        assert_eq!(out.value, Value::Null);
    }

    /// 脚本给 `agent()` 的 schema 也要跨边界（上游同样在 `checkBoundary` 里查它）。
    #[test]
    fn agent_schema_must_cross_the_json_boundary() {
        let err = run(
            "return await agent('x', { schema: { type: 'object', description: new Date(0) } })",
            None,
        )
        .unwrap_err();
        assert!(err.contains("non-plain object"), "{err}");
    }

    /// 顶层 args 与返回值都有体积上界（512KiB），超限在启动前/交付前就被拒。
    #[test]
    fn oversized_args_and_results_are_rejected() {
        let big = "x".repeat(super::MAX_BOUNDARY_BYTES + 1);
        let err = run("return 1", Some(serde_json::json!({ "blob": big }))).unwrap_err();
        assert!(err.contains("over the limit"), "{err}");

        let err = run(
            &format!(
                "return {{ blob: 'x'.repeat({}) }}",
                super::MAX_BOUNDARY_BYTES + 1
            ),
            None,
        )
        .unwrap_err();
        assert!(err.contains("over the limit"), "{err}");
    }

    /// 嵌套 `workflow()` 的 args 与返回值同样过边界。
    #[test]
    fn nested_workflow_values_cross_the_boundary() {
        let resolver: Arc<ResolveWorkflowFn> = Arc::new(|_p: &Value| {
            Ok(ResolvedWorkflow {
                name: "child".to_string(),
                body: "return 1".to_string(),
            })
        });
        let run_nested_body = |body: &str| {
            rt().block_on(run_script(WorkflowInputs {
                body: body.to_string(),
                args: None,
                limits: WorkflowLimits::default(),
                runner: runner_echo(),
                cancel: Arc::new(AtomicBool::new(false)),
                spent: Arc::new(AtomicU64::new(0)),
                progress: log(),
                schema: None,
                journal: None,
                control_sink: None,
                resolve_workflow: Some(resolver.clone()),
            }))
        };
        let err =
            run_nested_body("return await workflow('child', { when: new Date(0) })").unwrap_err();
        assert!(err.contains("non-plain object"), "{err}");
        // 合法 args 仍通过
        run_nested_body("return await workflow('child', { when: 0 })").unwrap();
    }
}
