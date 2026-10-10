//! autoresearch 作业：把同一个目标反复交给子代理迭代，直到达标或到轮次上限
//!
//! ## 形状
//!
//! 作业**不**自己实现"跑一轮子代理"：每一轮都走同一条**前台**派发路径
//! （`manager::dispatch_with_id`，`run_in_background = false`），
//! 首轮 `inherit_context = true` 从当前会话分叉，之后每轮 `resume` 同一个子代理记录——
//! "同一个子会话接着聊"由既有的 resume 机制表达，历史落在子会话文件里。
//!
//! 为什么不用 workflow 引擎：workflow 运行有墙钟兜底（`bridge::MAX_WALL_MS`），
//! 而 autoresearch 是长作业，会被那个兜底掐掉。
//!
//! ## 与主回合解耦
//!
//! 作业在扩展自己的任务里跑，**不随主回合的 Esc 一起死**：
//!
//! - 派发借 `workflow_owned`（该标志的既有语义是"自主运行、不随父级中止"）；
//! - 每轮把 `ctx.parent_abort` 换成一个新的、永不置位的信号——否则主回合 Esc 会经
//!   `run_linked` 把正在跑的那一轮一起掐掉。
//!
//! 停止只有两条路：`/autoresearch stop <id>`，或会话切换 / 扩展禁用时的 [`cancel_all`]。
//!
//! 解耦也意味着**结果不进主对话**：作业终结时只投一条人向通知（[`notify_settled`]），
//! 不注入 `Continuation`——上百轮的作业若每轮都往主会话灌 `<subagent_result>`，
//! 主对话与上下文都会被填满。要看某一轮走 `/agents result <agent_id>`。
//!
//! ## 上限与留存
//!
//! 活跃作业 4 个（第 5 个排队等额度）、保留 20 条、`--iterations` clamp 到 100
//! 作业表是内存态（重启即丢），但子会话本身落盘，历史不丢。
//!
//! ## 已知退化
//!
//! 每轮的 resume 目标取自 `manager` 的记录表：该表只保留最近 50 条已终结记录
//! （`MAX_FINISHED_RECORDS`）。极端情况下（同一会话里另有大量子代理收尾）作业的子记录
//! 可能被淘汰，下一轮 resume 会以"unknown agent id"失败——作业落 `Failed` 并在列表里
//! 显示原因，不会静默继续。

use super::{
    agent_types,
    config::SubagentConfig,
    fleet, manager,
    types::{AgentStatus, AgentType, SpawnRequest},
    util_notify, util_notify_spans,
};
use crate::{
    core::extensions::{DockLine, DockSpan, RichSpan, ToolExecCtx, UiNotifyLevel},
    extensions::{
        command_arg,
        util::{format_tokens, spinner_frame, truncate_chars},
    },
    modes::interactive::app::App,
    utils::{
        glyphs::{DEF_DONE, DEF_FAILED, DEF_QUEUED, DEF_STOPPED},
        time::format_duration,
    },
};
use std::{
    sync::{
        Arc, Mutex, MutexGuard, OnceLock,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use strum_macros::{EnumString, IntoStaticStr};
use tokio::runtime::Handle;

/// 作业 id（从 1 递增；`/autoresearch stop` 与列表都用它）。
pub(crate) type JobId = u64;

/// 同时**活跃**（排队 + 运行）的作业上限；第 5 个起排队等额度。
const MAX_ACTIVE: usize = 4;
/// 作业表保留条数上限；超出时淘汰**已终结**的最旧条目（没有已终结的就不再淘汰）。
const MAX_RETAINED: usize = 20;
/// `--iterations` 的硬上限（与 kiss 的 `MAX_ITERATIONS` 同值）。
pub(crate) const MAX_ITERATIONS: u32 = 100;
/// 等活跃额度时的轮询间隔。
const SLOT_POLL: Duration = Duration::from_millis(100);
/// dock 段里最多列几个未终结作业（更多的仍在 `/autoresearch` 列表里）。
const MAX_DOCK_JOBS: usize = 4;
/// 作业用的 agent 类型：父级孪生（追加桥接段而非替换提示词），与工作流的缺省一致。
const JOB_AGENT_TYPE: &str = "general-purpose";
/// 完成标记：回答里出现它即收工（大小写不敏感）。
const COMPLETION_MARKER: &str = "[goal-complete]";

/// 下一个作业 id。
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
/// 已占用的活跃额度（上限 [`MAX_ACTIVE`]）。
static ACTIVE: AtomicUsize = AtomicUsize::new(0);

/// 作业状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumString, IntoStaticStr)]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
pub(crate) enum JobStatus {
    /// 已建，等活跃额度
    Queued,
    /// 正在迭代
    Running,
    /// 达标（回答出现完成标记）或跑到 `--iterations` 上限
    Completed,
    /// 子代理出错、类型解析失败或 resume 目标丢失
    Failed,
    /// 用户 `/autoresearch stop`、会话切换或扩展被禁用
    Stopped,
}

impl JobStatus {
    /// 状态词（列表展示与测试断言用），由 strum 派生。
    pub(crate) fn as_str(self) -> &'static str {
        self.into()
    }

    /// 是否已终结（不再占活跃额度、dock 不再列）。
    pub(crate) fn is_finished(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Stopped)
    }

    /// 状态图标（运行中用 spinner 帧，因此每帧渲染都会推进）。
    fn icon(self) -> &'static str {
        match self {
            Self::Queued => DEF_QUEUED,
            Self::Running => spinner_frame(),
            Self::Completed => DEF_DONE,
            Self::Failed => DEF_FAILED,
            Self::Stopped => DEF_STOPPED,
        }
    }

    /// 列表行的（主题色键, 缺省色）。
    fn color(self) -> (&'static str, &'static str) {
        match self {
            Self::Running => ("accent", "#8abeb7"),
            Self::Completed => ("success", "#b5bd68"),
            Self::Failed => ("error", "#cc6666"),
            Self::Queued | Self::Stopped => ("muted", "#999999"),
        }
    }
}

/// 一条作业的只读快照（dock / 列表 / 测试都读它，不碰作业内部状态）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct JobSnapshot {
    /// 作业 id
    pub id: JobId,
    /// 用户写的目标（原样保留：提示词与列表都用它）
    pub goal: String,
    /// 当前状态
    pub status: JobStatus,
    /// 已开始的轮次（0 = 还没跑第一轮）
    pub iteration: u32,
    /// `--iterations` 上限（None = 无上限，靠完成标记收工）
    pub limit: Option<u32>,
    /// 累计 token（各轮子代理 `display_total` 的累加）
    pub tokens: u64,
    /// 从启动到现在的耗时（终结后固定）
    pub elapsed: Duration,
    /// 当前/最近一轮的子代理记录 id（resume 目标；还没派发时为 None）
    pub agent_id: Option<String>,
    /// 最近一轮的回答（完成标记的判定依据）
    pub latest_result: Option<String>,
    /// 失败原因（`Failed` 时必有）
    pub error: Option<String>,
}

/// 作业表里的一条（可变状态由 [`jobs`] 的互斥锁保护）。
struct Job {
    /// 作业 id
    id: JobId,
    /// 用户写的目标
    goal: String,
    /// `--iterations` 上限（None = 无上限）
    limit: Option<u32>,
    /// 当前状态
    status: JobStatus,
    /// 已开始的轮次
    iteration: u32,
    /// 累计 token
    tokens: u64,
    /// 启动时刻
    started: Instant,
    /// 终结时刻（None = 还在跑）
    finished_at: Option<Instant>,
    /// 当前/最近一轮的子代理记录 id
    agent_id: Option<String>,
    /// 最近一轮的回答
    latest_result: Option<String>,
    /// 失败原因
    error: Option<String>,
    /// 停止信号：置位后作业在下一个检查点收尾，正在跑的那一轮另由 `manager::stop` 掐掉
    cancel: Arc<AtomicBool>,
}

impl Job {
    /// 快照（耗时在终结后固定，避免列表里的数字一直涨）。
    fn snapshot(&self) -> JobSnapshot {
        JobSnapshot {
            id: self.id,
            goal: self.goal.clone(),
            status: self.status,
            iteration: self.iteration,
            limit: self.limit,
            tokens: self.tokens,
            elapsed: self
                .finished_at
                .unwrap_or_else(Instant::now)
                .duration_since(self.started),
            agent_id: self.agent_id.clone(),
            latest_result: self.latest_result.clone(),
            error: self.error.clone(),
        }
    }
}

/// 作业表（进程级单例）。
fn jobs() -> &'static Mutex<Vec<Job>> {
    /// 作业表：只有 [`lock_jobs`] 取它，且**持锁期间不回调 `manager`**（锁序固定）。
    static S: OnceLock<Mutex<Vec<Job>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(Vec::new()))
}

/// 取作业表锁；持锁线程曾 panic 时取中毒锁内的数据继续用。
fn lock_jobs() -> MutexGuard<'static, Vec<Job>> {
    jobs().lock().unwrap_or_else(|e| e.into_inner())
}

/// 改一条作业（id 不在表里时什么都不做）。
fn update(id: JobId, f: impl FnOnce(&mut Job)) {
    let mut list = lock_jobs();
    if let Some(job) = list.iter_mut().find(|j| j.id == id) {
        f(job);
    }
}

/// 全部作业的快照（表序 = 创建序）。
pub(crate) fn snapshots() -> Vec<JobSnapshot> {
    lock_jobs().iter().map(Job::snapshot).collect()
}

/// 还有未终结的作业（dock 据此请求持续重绘）。
pub(crate) fn has_live() -> bool {
    lock_jobs().iter().any(|j| !j.status.is_finished())
}

/// 启动一个作业（`/autoresearch <goal> [--iterations N]` 的落地）。
///
/// 需要本会话已有执行上下文（[`fleet::cached_ctx`]）：拿不到就报错，不静默丢作业。
/// 返回作业 id；目标为空或没有执行上下文时返回 `Err`（消息可直接展示给用户）。
pub(crate) fn start(goal: &str, limit: Option<u32>) -> Result<JobId, String> {
    let goal = goal.trim();
    if goal.is_empty() {
        return Err("the job goal cannot be empty".to_string());
    }

    let Some((ctx, handle)) = fleet::cached_ctx() else {
        return Err(
            "cannot start an autoresearch job: this session has no execution context yet"
                .to_string(),
        );
    };

    Ok(start_with(ctx, handle, goal, limit))
}

/// 用给定的上下文与运行时句柄启动作业（[`start`] 与端到端测试共用）。
///
/// 上下文被**整个作业持有**（每轮克隆）：作业不依赖 `fleet` 缓存刷新，因此也不受
/// "缓存被清掉"影响；代价是作业期间切换模型不影响已在跑的作业——与 kiss 一致
/// （子会话的模型在分叉时就定了）。
pub(crate) fn start_with(
    ctx: ToolExecCtx,
    handle: Handle,
    goal: &str,
    limit: Option<u32>,
) -> JobId {
    let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
    let limit = limit.map(|limit| limit.clamp(1, MAX_ITERATIONS));
    let cancel = Arc::new(AtomicBool::new(false));

    {
        let mut list = lock_jobs();
        list.push(Job {
            id,
            goal: goal.to_string(),
            limit,
            status: JobStatus::Queued,
            iteration: 0,
            tokens: 0,
            started: Instant::now(),
            finished_at: None,
            agent_id: None,
            latest_result: None,
            error: None,
            cancel: cancel.clone(),
        });
        prune(&mut list);
    }

    handle.spawn(run(id, ctx, goal.to_string(), limit, cancel));
    id
}

/// 淘汰超出保留上限的**已终结**作业（最旧的先走）。
fn prune(list: &mut Vec<Job>) {
    while list.len() > MAX_RETAINED {
        let Some(position) = list.iter().position(|job| job.status.is_finished()) else {
            break;
        };
        list.remove(position);
    }
}

/// 作业主体：等额度 → 逐轮派发 → 判完成 / 到上限 / 失败 / 被停。
async fn run(
    id: JobId,
    ctx: ToolExecCtx,
    goal: String,
    limit: Option<u32>,
    cancel: Arc<AtomicBool>,
) {
    if !acquire_slot(&cancel).await {
        settle(id, JobStatus::Stopped, None);
        return;
    }

    let _slot = SlotGuard;
    update(id, |job| job.status = JobStatus::Running);

    let mut iteration = 1_u32;
    let mut child: Option<String> = None;

    loop {
        if cancel.load(Ordering::SeqCst) {
            settle(id, JobStatus::Stopped, None);
            return;
        }
        update(id, |job| job.iteration = iteration);

        let Some((ty, cfg)) = resolve_type(&ctx.cwd) else {
            settle(
                id,
                JobStatus::Failed,
                Some(format!("agent type {JOB_AGENT_TYPE} is not available")),
            );
            return;
        };

        // 本轮上下文：换掉 parent_abort，作业不随主回合 Esc 一起死（见模块文档）。
        let mut iter_ctx = ctx.clone();
        iter_ctx.parent_abort = Arc::new(AtomicBool::new(false));

        let mut req = SpawnRequest::from_type(
            &ty,
            &cfg,
            iteration_prompt(&goal, iteration, limit),
            format!("autoresearch: {}", truncate_chars(&goal, 40)),
        );
        req.run_in_background = false;
        req.inherit_context = iteration == 1;
        req.persist_session = ty.persist_session.unwrap_or(true);
        req.resume = child.clone();
        req.workflow_owned = true;

        // 记录 id 在**开始跑之前**就写进作业表：否则 `/autoresearch stop`（或会话切换的
        // `cancel_all`）在那一轮跑到一半时拿到的是 None，停不掉它。
        let on_id: Option<Arc<dyn Fn(String) + Send + Sync>> =
            Some(Arc::new(move |agent_id: String| {
                update(id, |job| job.agent_id = Some(agent_id));
            }));

        // 派发与停止信号竞争：停止可能在**这一轮跑着的时候**到达（作业表是原子标志，
        // 没有异步唤醒器，所以用轮询等）。派发 future 被丢掉不会杀掉已起来的 run
        // （manager 的前台路径在独立任务里跑），所以这里要显式 `manager::stop` 它。
        let cancel_wait = cancel.clone();
        let mut dispatch = Box::pin(manager::dispatch_with_id(&iter_ctx, &ty, req, on_id));
        let dispatched = tokio::select! {
            result = &mut dispatch => match result {
                Ok(dispatched) => dispatched,
                Err(e) => {
                    settle(id, JobStatus::Failed, Some(e));
                    return;
                }
            },
            () = wait_cancel(&cancel_wait) => {
                if let Some(agent_id) = agent_of(id) {
                    _ = manager::stop(&agent_id);
                }
                settle(id, JobStatus::Stopped, None);
                return;
            }
        };
        let agent_id = dispatched.id;
        let record = manager::record(&agent_id);
        let status = record.as_ref().map_or(AgentStatus::Error, |r| r.status);
        let result = record.as_ref().and_then(|r| r.result.clone());
        let tokens = record.as_ref().map_or(0, |r| r.usage.display_total());

        update(id, |job| {
            job.agent_id = Some(agent_id.clone());
            job.tokens = job.tokens.saturating_add(tokens);
            job.latest_result = result.clone();
        });
        child = Some(agent_id);

        if cancel.load(Ordering::SeqCst)
            || matches!(status, AgentStatus::Aborted | AgentStatus::Stopped)
        {
            settle(id, JobStatus::Stopped, None);
            return;
        }
        if record.is_none() || status == AgentStatus::Error {
            settle(
                id,
                JobStatus::Failed,
                Some(result.unwrap_or_else(|| "the sub-agent ended with an error".to_string())),
            );
            return;
        }
        if result.as_deref().is_some_and(contains_completion_marker) {
            settle(id, JobStatus::Completed, None);
            return;
        }
        if limit == Some(iteration) {
            settle(id, JobStatus::Completed, None);
            return;
        }

        iteration = iteration.saturating_add(1);
    }
}

/// 写终态并固定耗时（`error` 为 None 时保留原值：失败原因只在真失败时写），
/// 随后按新终态投一条人向通知（见 [`notify_settled`]）。
fn settle(id: JobId, status: JobStatus, error: Option<String>) {
    // 快照在锁内取、通知在锁外发：`util_notify` 会碰 UI 通道，不在持作业表锁时回调出去。
    let snapshot = {
        let mut list = lock_jobs();
        let Some(job) = list.iter_mut().find(|job| job.id == id) else {
            return;
        };

        job.status = status;
        job.finished_at = Some(Instant::now());

        if error.is_some() {
            job.error = error;
        }

        job.snapshot()
    };

    notify_settled(&snapshot);
}

/// 作业终结时给人看的通知：dock 段会随终结消失，不发就等于静默收场。
///
/// 只发通知，**不往主会话注入消息**：作业与主回合解耦（见模块文档），
/// 上百轮的作业若每轮都注入 `<subagent_result>` 会把主对话灌满；
/// 要看某一轮的完整回答走 `/agents result <agent_id>`。
///
/// `Stopped` 不发：那条路要么是用户自己按的 `/autoresearch stop`（命令已经回过话），
/// 要么是会话切换 / 扩展禁用，再弹一条只是噪声。
fn notify_settled(snapshot: &JobSnapshot) {
    let goal = truncate_chars(&snapshot.goal, 40);
    match snapshot.status {
        JobStatus::Completed => util_notify(
            &format!(
                "autoresearch job #{} completed · {} · {} tok · {} · {goal} · /agents result {} for the answer",
                snapshot.id,
                progress(snapshot),
                format_tokens(snapshot.tokens),
                format_duration(snapshot.elapsed.as_millis() as u64),
                snapshot.agent_id.as_deref().unwrap_or("(no record)"),
            ),
            UiNotifyLevel::Success,
        ),
        JobStatus::Failed => util_notify(
            &format!(
                "autoresearch job #{} failed · {} · {goal} · error: {}",
                snapshot.id,
                progress(snapshot),
                truncate_chars(snapshot.error.as_deref().unwrap_or("unknown error"), 120),
            ),
            UiNotifyLevel::Warning,
        ),
        JobStatus::Queued | JobStatus::Running | JobStatus::Stopped => {}
    }
}

/// 占一个活跃额度（满额时轮询等待）；被停则放弃且**不占**额度。
async fn acquire_slot(cancel: &AtomicBool) -> bool {
    loop {
        if cancel.load(Ordering::SeqCst) {
            return false;
        }
        let acquired = ACTIVE
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n < MAX_ACTIVE).then_some(n + 1)
            })
            .is_ok();
        if acquired {
            return true;
        }
        tokio::time::sleep(SLOT_POLL).await;
    }
}

/// 活跃额度守卫：作业一结束就归还（正常收尾、失败、panic 展开都归还）。
struct SlotGuard;

impl Drop for SlotGuard {
    fn drop(&mut self) {
        // 饱和减：测试接缝会重置计数，重复归还不能让 `usize` 回绕成
        // `usize::MAX`（那样所有后续作业都会永远排队）
        _ = ACTIVE.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
            Some(n.saturating_sub(1))
        });
    }
}

/// 等停止信号（轮询：作业表用的是 [`AtomicBool`]，没有异步唤醒器）。
async fn wait_cancel(cancel: &AtomicBool) {
    while !cancel.load(Ordering::SeqCst) {
        tokio::time::sleep(SLOT_POLL).await;
    }
}

/// 一条作业当前的子代理记录 id（还没派发时为 None）。
fn agent_of(id: JobId) -> Option<String> {
    lock_jobs()
        .iter()
        .find(|job| job.id == id)
        .and_then(|job| job.agent_id.clone())
}

/// 解析作业用的 agent 类型与配置（类型被删/禁用时 None，作业落 `Failed`）。
fn resolve_type(cwd: &str) -> Option<(AgentType, SubagentConfig)> {
    let cfg = manager::config();
    let roster = agent_types::discover(cwd);
    agent_types::resolve(&roster.types, JOB_AGENT_TYPE)
        .ok()
        .cloned()
        .map(|ty| (ty, cfg))
}

/// 停止一个作业：置停止信号，并掐掉正在跑的那一轮。
///
/// 已终结的作业返回 `Err`（消息可直接展示给用户）；作业 id 不存在同样 `Err`。
pub(crate) fn stop(id: JobId) -> Result<(), String> {
    let agent_id = {
        let mut list = lock_jobs();
        let Some(job) = list.iter_mut().find(|j| j.id == id) else {
            return Err(format!("unknown autoresearch job: {id}"));
        };
        if job.status.is_finished() {
            return Err(format!(
                "autoresearch job #{id} is already {}",
                job.status.as_str()
            ));
        }
        job.cancel.store(true, Ordering::SeqCst);
        job.agent_id.clone()
    };

    // 锁已释放再碰 manager（锁序固定：作业表 → 注册表，绝不反向）。
    if let Some(agent_id) = agent_id {
        _ = manager::stop(&agent_id);
    }
    Ok(())
}

/// 停掉所有未终结作业（会话切换 / 扩展被禁用）。
///
/// 已终结的作业原样保留：它们的历史不该因为切会话而消失（`/new` 后列表还在）。
pub(crate) fn cancel_all() {
    let running: Vec<String> = {
        let mut list = lock_jobs();
        let mut running = Vec::new();
        for job in list.iter_mut().filter(|j| !j.status.is_finished()) {
            job.cancel.store(true, Ordering::SeqCst);
            if let Some(agent_id) = job.agent_id.clone() {
                running.push(agent_id);
            }
        }
        running
    };

    for agent_id in running {
        _ = manager::stop(&agent_id);
    }
}

/// 回答里是否有完成标记（大小写不敏感）。
fn contains_completion_marker(result: &str) -> bool {
    result.to_ascii_lowercase().contains(COMPLETION_MARKER)
}

/// 本轮提示词
fn iteration_prompt(goal: &str, iteration: u32, limit: Option<u32>) -> String {
    let position = limit.map_or_else(
        || format!("iteration {iteration}"),
        |limit| format!("iteration {iteration} of {limit}"),
    );

    let common = format!(
        "You are in {position} for this goal:\n\n{goal}\n\nWork directly in the shared repository. Make one useful, verified unit of progress. Do not ask for permission. End the answer with exactly [goal-complete] if the goal is fully achieved and verified. Otherwise end with exactly [continue]."
    );
    format!(
        "Autoresearch job. On the first iteration, establish a repeatable baseline and success metric. On each iteration, test one small idea, run the same verification, keep an improvement, and revert a regression. Protect existing behavior with focused tests. Record the measured result and the decision in the answer.\n\n{common}"
    )
}

/// 解析 `/autoresearch <goal> [--iterations N]`。
///
/// 规则：只认**结尾**的 `--iterations N`（`N` 必须是单个词），
/// 其余整段都是目标（`-n` / `--iterations=N` 都算目标的一部分，不做特例）。
/// 返回 `(目标, 轮次上限)`；目标为空、`N` 不是正整数或 `N` 后面还有别的词时返回 `Err`。
pub(crate) fn parse_request(arg: &str) -> Result<(String, Option<u32>), String> {
    let arg = arg.trim();
    let (goal, limit) = match arg.rsplit_once("--iterations") {
        Some((goal, value)) => {
            let value = value.trim();
            if value.is_empty() || value.split_whitespace().count() != 1 {
                return Err("use --iterations N at the end of the job goal".to_string());
            }
            let limit = value
                .parse::<u32>()
                .ok()
                .filter(|limit| *limit > 0)
                .ok_or_else(|| "iterations must be a positive number".to_string())?;
            (goal.trim(), Some(limit))
        }
        None => (arg, None),
    };

    if goal.is_empty() {
        return Err("the job goal cannot be empty".to_string());
    }
    Ok((goal.to_string(), limit))
}

/// 解析 `stop` 的 id 写法：`3` / `#3` / `autoresearch-3` 都认。
fn parse_job_id(raw: &str) -> Option<JobId> {
    let raw = raw.trim();
    let raw = raw.strip_prefix('#').unwrap_or(raw);
    let raw = raw.strip_prefix("autoresearch-").unwrap_or(raw);
    raw.parse::<JobId>().ok().filter(|id| *id > 0)
}

/// 轮次进度文本：`2/20`（无上限时只写轮次）。
fn progress(snapshot: &JobSnapshot) -> String {
    match snapshot.limit {
        Some(limit) => format!("{}/{}", snapshot.iteration, limit),
        None => snapshot.iteration.to_string(),
    }
}

/// dock 段：标题 + 未终结作业各一行（没有未终结作业时返回空 = 不占位）。
pub(crate) fn dock_lines() -> Vec<DockLine> {
    let (active, rows) = {
        let list = lock_jobs();
        let live: Vec<JobSnapshot> = list
            .iter()
            .filter(|job| !job.status.is_finished())
            .map(Job::snapshot)
            .collect();
        let active = live.len();
        (
            active,
            live.into_iter().take(MAX_DOCK_JOBS).collect::<Vec<_>>(),
        )
    };

    if rows.is_empty() {
        return Vec::new();
    }

    let mut lines: Vec<DockLine> = vec![vec![
        DockSpan::new("accent", "#8abeb7", "Autoresearch"),
        DockSpan::new(
            "dim",
            "#666666",
            format!(" · {active} job{}", if active == 1 { "" } else { "s" }),
        ),
    ]];
    for snapshot in rows {
        lines.push(dock_line(&snapshot));
    }
    lines
}

/// dock 一行：`⠹ #1 2/20 · 12.3K tok · 3m12s · <目标截断>`。
fn dock_line(snapshot: &JobSnapshot) -> DockLine {
    let (key, fallback) = snapshot.status.color();
    vec![
        DockSpan::new(key, fallback, format!("{} ", snapshot.status.icon())),
        DockSpan::new("accent", "#8abeb7", format!("#{}", snapshot.id)),
        DockSpan::new("dim", "#666666", format!(" {}", progress(snapshot))),
        DockSpan::new(
            "dim",
            "#666666",
            format!(
                " · {} tok · {}",
                format_tokens(snapshot.tokens),
                format_duration(snapshot.elapsed.as_millis() as u64)
            ),
        ),
        DockSpan::new(
            "text",
            "#d0d0d0",
            format!(" · {}", truncate_chars(&snapshot.goal, 40)),
        ),
    ]
}

/// 列表通知的富文本（`/autoresearch` 无参时投递）。
///
/// 每行一条作业：状态图标 + `#id` + 进度 + token/耗时 + 目标；选中标记留给有选择器的视图，
/// 这里只做只读展示。失败与最后一轮的结果各补一行。
fn list_spans() -> Vec<RichSpan> {
    let list = snapshots();
    if list.is_empty() {
        return vec![RichSpan::plain(
            "No autoresearch jobs have run in this session.",
        )];
    }

    let live = list
        .iter()
        .filter(|snapshot| !snapshot.status.is_finished())
        .count();
    let mut spans = vec![RichSpan::plain(format!(
        "Autoresearch · {live} active · {} total\n",
        list.len()
    ))];

    for snapshot in &list {
        let (key, _) = snapshot.status.color();
        spans.push(RichSpan::fg(key, format!("{} ", snapshot.status.icon())));
        spans.push(RichSpan::plain(format!(
            "#{} {} · {} · {} tok · {} · {}\n",
            snapshot.id,
            snapshot.status.as_str(),
            progress(snapshot),
            crate::extensions::util::format_tokens(snapshot.tokens),
            format_duration(snapshot.elapsed.as_millis() as u64),
            truncate_chars(&snapshot.goal, 60),
        )));

        if let Some(error) = snapshot.error.as_deref() {
            spans.push(RichSpan::plain(format!(
                "   error: {}\n",
                truncate_chars(error, 120)
            )));
        } else if let Some(result) = snapshot.latest_result.as_deref() {
            spans.push(RichSpan::plain(format!(
                "   last: {}\n",
                truncate_chars(result, 120)
            )));
        }
    }

    spans.push(RichSpan::plain(
        "Stop one with /autoresearch stop <id> (e.g. /autoresearch stop 3).",
    ));
    spans
}

/// `/autoresearch [<goal> [--iterations N] | stop <id>]`。
///
/// 无参列出全部作业；`stop <id>` 停一个未终结作业；其余按"目标 + 可选轮次上限"启动。
/// 返回值与其它扩展命令一致：恒为 `false`（不退出 TUI）。
pub(crate) fn command(_app: &mut App, raw: &str) -> bool {
    let arg = command_arg(raw).trim();
    if arg.is_empty() {
        util_notify_spans(list_spans(), UiNotifyLevel::Info);
        return false;
    }

    let (head, rest) = match arg.split_once(char::is_whitespace) {
        Some((head, rest)) => (head, rest.trim()),
        None => (arg, ""),
    };

    if head == "stop" {
        if rest.is_empty() {
            util_notify("usage: /autoresearch stop <id>", UiNotifyLevel::Warning);
            return false;
        }
        let Some(id) = parse_job_id(rest) else {
            util_notify(
                &format!("unknown autoresearch job: {rest}"),
                UiNotifyLevel::Warning,
            );
            return false;
        };
        match stop(id) {
            Ok(()) => util_notify(
                &format!("stopping autoresearch job #{id}"),
                UiNotifyLevel::Info,
            ),
            Err(e) => util_notify(&e, UiNotifyLevel::Warning),
        }
        return false;
    }

    let (goal, limit) = match parse_request(arg) {
        Ok(parsed) => parsed,
        Err(e) => {
            util_notify(&e, UiNotifyLevel::Warning);
            return false;
        }
    };

    match start(&goal, limit) {
        Ok(id) => {
            let schedule = match limit {
                Some(limit) => format!("up to {limit} iterations"),
                None => "no iteration limit".to_string(),
            };
            util_notify(
                &format!(
                    "started autoresearch job #{id} · {schedule} · /autoresearch lists jobs, /agents shows its sub-agent"
                ),
                UiNotifyLevel::Info,
            );
        }
        Err(e) => util_notify(&e, UiNotifyLevel::Warning),
    }
    false
}

/// 清空作业表（测试接缝：`cancel_all` 只置停止信号，作业表本身要显式清）。
#[cfg(test)]
pub(crate) fn reset_for_test() {
    lock_jobs().clear();
    ACTIVE.store(0, Ordering::SeqCst);
}

/// 一条作业的快照（不存在时 None）。测试接缝：生产路径只经 [`snapshots`] 与 dock 渲染。
#[cfg(test)]
pub(crate) fn snapshot(id: JobId) -> Option<JobSnapshot> {
    lock_jobs().iter().find(|j| j.id == id).map(Job::snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::extensions::{
        ExtensionUiRequest, SubAgentControls, SubAgentRunner, SubAgentSpec,
    };
    use futures_util::future::BoxFuture;

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Runtime::new().unwrap()
    }

    /// 脚本化 runner 的结局。
    enum Reply {
        /// 正常回答这段文本
        Text(String),
        /// 以这条错误失败
        Fail(String),
        /// 挂起直到被 abort（验证停止路径）
        Hold,
    }

    /// 脚本化 runner：每轮由 `make_sub_agent` 新建一个，共享的捕获器记下每轮输入。
    struct ScriptRunner {
        /// 本轮的结局
        reply: Reply,
        /// 汇报给 manager 的子会话路径（resume 的前提）
        session: Option<String>,
        /// 中止信号：与 `controls()` 交出去的是同一个，`manager::stop` 置位后本轮结束
        abort: Arc<AtomicBool>,
        /// 捕获器：把本轮收到的提示词记下来
        prompts: Arc<Mutex<Vec<String>>>,
    }

    impl SubAgentRunner for ScriptRunner {
        fn controls(&self) -> SubAgentControls {
            SubAgentControls {
                steer: Arc::new(|_message: String| {}),
                abort: self.abort.clone(),
                session_path: self.session.clone(),
            }
        }

        fn run(&mut self, prompt: String) -> BoxFuture<'_, Result<String, String>> {
            self.prompts.lock().unwrap().push(prompt);
            let reply = match &self.reply {
                Reply::Text(text) => Ok(text.clone()),
                Reply::Fail(error) => Err(error.clone()),
                Reply::Hold => Err("Operation aborted".to_string()),
            };
            let abort = self.abort.clone();
            let hold = matches!(self.reply, Reply::Hold);
            Box::pin(async move {
                if hold {
                    // 挂到被 manager 中止（`manager::stop` 置位的就是这个标志）
                    while !abort.load(Ordering::SeqCst) {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                }
                reply
            })
        }
    }

    /// 一个作业的输入捕获器（每轮的提示词 / resume 目标 / inherit_context）。
    #[derive(Default)]
    struct Capture {
        /// 每轮提示词
        prompts: Arc<Mutex<Vec<String>>>,
        /// 每轮物化 spec 的 `resume_session_path`（None = 首轮）
        resumes: Arc<Mutex<Vec<Option<String>>>>,
        /// 每轮物化 spec 的 `inherit_context`
        inherits: Arc<Mutex<Vec<bool>>>,
    }

    /// 测试用上下文：脚本化 runner（按 `replies` 次序回） + 捕获器。
    ///
    /// runner 汇报一个假的子会话路径：`resume` 要求记录带 `session_path`，没有它
    /// 第二轮的派发会直接以"no persisted session to resume"失败——那样测的就不是迭代循环了。
    fn script_ctx(replies: Vec<Reply>) -> (ToolExecCtx, Capture) {
        let capture = Capture::default();
        let index = Arc::new(AtomicUsize::new(0));
        let session = "/tmp/autoresearch-child.jsonl".to_string();
        let (prompts, resumes, inherits) = (
            capture.prompts.clone(),
            capture.resumes.clone(),
            capture.inherits.clone(),
        );

        let make: Arc<crate::core::extensions::MakeSubAgentFn> =
            Arc::new(move |spec: SubAgentSpec| {
                resumes
                    .lock()
                    .unwrap()
                    .push(spec.resume_session_path.clone());
                inherits.lock().unwrap().push(spec.inherit_context);
                let call = index.fetch_add(1, Ordering::SeqCst);
                let reply = match replies.get(call) {
                    Some(Reply::Text(text)) => Reply::Text(text.clone()),
                    Some(Reply::Fail(error)) => Reply::Fail(error.clone()),
                    Some(Reply::Hold) | None => Reply::Hold,
                };
                Ok(Box::new(ScriptRunner {
                    reply,
                    session: Some(session.clone()),
                    abort: Arc::new(AtomicBool::new(false)),
                    prompts: prompts.clone(),
                }) as Box<dyn SubAgentRunner>)
            });

        let ctx = ToolExecCtx {
            execute_tool: crate::core::extensions::unavailable_tool_exec(),
            parent_tool_call_id: None,
            nested_calls: Default::default(),
            session_branch_entries: Default::default(),
            script_tools: Default::default(),
            cwd: std::env::temp_dir().to_string_lossy().to_string(),
            make_sub_agent: make,
            parent_abort: Arc::new(AtomicBool::new(false)),
            agent_id: None,
            depth: 0,
            parent_model: None,
            script_call: false,
        };
        (ctx, capture)
    }

    /// 在运行时里跑一个作业并等它终结（超时即失败）。
    fn run_job(replies: Vec<Reply>, goal: &str, limit: Option<u32>) -> (JobSnapshot, Capture) {
        let (ctx, capture) = script_ctx(replies);
        let runtime = rt();
        let snapshot = runtime.block_on(async move {
            let id = start_with(ctx, tokio::runtime::Handle::current(), goal, limit);
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if let Some(snapshot) = snapshot(id)
                    && snapshot.status.is_finished()
                {
                    return snapshot;
                }
                assert!(Instant::now() < deadline, "作业没有在预期时间内终结");
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        (snapshot, capture)
    }

    /// 测试环境：跨测试串行锁 + 临时 agent_dir（作业会写子会话记录）。
    ///
    /// 字段顺序即释放顺序：先收掉临时目录，再放开 manager 锁，最后放开 `AUTH_TEST_LOCK`
    /// （与获取顺序相反）。
    struct Setup {
        /// 临时 agent_dir
        _dir: crate::test_support::AgentDirGuard,
        /// 跨测试串行锁
        _lock: super::manager::TestLock,
        /// 全局态锁：本用例会清空**进程级**的 UI 队列（作业终结通知），
        /// 而 plan-mode 等用例也往同一队列里写——不串行就会互相 take 走对方的请求。
        /// 锁序与 `agent_session` 的子代理用例一致：`AUTH_TEST_LOCK` → `manager::test_lock`。
        _auth: std::sync::MutexGuard<'static, ()>,
    }

    impl Drop for Setup {
        /// 用例结束时把本用例排进全局 UI 队列的请求（作业终结通知）取空。
        ///
        /// 队列是进程级的：留着就是下一个用例的脏数据（其它模块的用例会 `take_pending_ui`
        /// 抓到不属于自己的通知）。在锁仍持有（Drop 早于字段释放）时清，避免与并行用例抢。
        fn drop(&mut self) {
            while crate::core::extensions::take_pending_ui().is_some() {}
        }
    }

    fn setup() -> Setup {
        let auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let lock = super::manager::test_lock();
        let dir = crate::test_support::AgentDirGuard::temp();
        manager::reset_all();
        reset_for_test();
        while crate::core::extensions::take_pending_ui().is_some() {}
        Setup {
            _dir: dir,
            _lock: lock,
            _auth: auth,
        }
    }

    /// 取走待投递的 UI 请求：返回人向通知（文本, 级别）与是否出现过 `Continuation`
    /// （后者是「往主会话注入消息」的唯一形式，作业不该产生它）。
    fn drain_ui() -> (Vec<(String, UiNotifyLevel)>, bool) {
        let mut notes: Vec<(String, UiNotifyLevel)> = Vec::new();
        let mut continuation = false;
        while let Some(request) = crate::core::extensions::take_pending_ui() {
            match request {
                ExtensionUiRequest::Notify { text, level } => notes.push((text, level)),
                ExtensionUiRequest::NotifyRich { spans, level } => notes.push((
                    spans.into_iter().map(|span| span.text).collect::<String>(),
                    level,
                )),
                ExtensionUiRequest::Continuation { .. } => continuation = true,
                _ => {}
            }
        }
        (notes, continuation)
    }

    /// 同 [`drain_ui`]，但等到至少一条通知落地为止（`settle` 先落状态、再投通知，
    /// 两者之间有一个窗口，而测试运行时是多线程的）。
    fn drain_settled_ui() -> (Vec<(String, UiNotifyLevel)>, bool) {
        rt().block_on(async {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let drained = drain_ui();
                if !drained.0.is_empty() || Instant::now() >= deadline {
                    return drained;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
    }

    #[test]
    fn prompts_define_progress_and_completion() {
        let prompt = iteration_prompt("make it faster", 2, Some(10));
        assert!(prompt.contains("iteration 2 of 10"), "{prompt}");
        assert!(prompt.contains("[goal-complete]"), "{prompt}");
        assert!(prompt.contains("[continue]"), "{prompt}");
    }

    #[test]
    fn unlimited_prompts_do_not_claim_a_limit() {
        let prompt = iteration_prompt("fix tests", 2, None);
        assert!(prompt.contains("iteration 2 for this goal"), "{prompt}");
        assert!(!prompt.contains("iteration 2 of"), "{prompt}");
    }

    #[test]
    fn prompts_ask_for_a_baseline_and_a_regression_revert() {
        let prompt = iteration_prompt("make it faster", 1, None);
        assert!(prompt.contains("baseline"), "{prompt}");
        assert!(prompt.contains("success metric"), "{prompt}");
        assert!(prompt.contains("revert a regression"), "{prompt}");
        assert!(prompt.contains("focused tests"), "{prompt}");
    }

    #[test]
    fn completion_marker_is_case_insensitive() {
        assert!(contains_completion_marker("done\n[GOAL-COMPLETE]"));
        assert!(contains_completion_marker("[goal-complete]"));
        assert!(!contains_completion_marker("keep going\n[continue]"));
    }

    #[test]
    fn parse_request_reads_a_trailing_iterations_flag() {
        let (goal, limit) = parse_request("reduce render time --iterations 20").unwrap();
        assert_eq!(goal, "reduce render time");
        assert_eq!(limit, Some(20));

        let (goal, limit) = parse_request("  no limit here  ").unwrap();
        assert_eq!(goal, "no limit here");
        assert_eq!(limit, None);
    }

    #[test]
    fn parse_request_rejects_bad_limits_and_empty_goals() {
        assert_eq!(
            parse_request("--iterations 0").unwrap_err(),
            "iterations must be a positive number"
        );
        assert_eq!(
            parse_request("x --iterations 0").unwrap_err(),
            "iterations must be a positive number"
        );
        assert_eq!(
            parse_request("x --iterations nope").unwrap_err(),
            "iterations must be a positive number"
        );
        assert_eq!(
            parse_request("x --iterations 5 6").unwrap_err(),
            "use --iterations N at the end of the job goal"
        );
        assert_eq!(
            parse_request("x --iterations").unwrap_err(),
            "use --iterations N at the end of the job goal"
        );
        assert_eq!(
            parse_request("   ").unwrap_err(),
            "the job goal cannot be empty"
        );
    }

    #[test]
    fn parse_job_id_accepts_the_documented_shapes() {
        assert_eq!(parse_job_id("3"), Some(3));
        assert_eq!(parse_job_id("#3"), Some(3));
        assert_eq!(parse_job_id("autoresearch-3"), Some(3));
        assert_eq!(parse_job_id("0"), None);
        assert_eq!(parse_job_id("abc"), None);
    }

    /// 核心链路：派发 → 读记录 → 判定 → 收敛。第 1 轮 `[continue]`，
    /// 第 2 轮（走 resume 的同一子会话）`[goal-complete]` → 作业落 `Completed`。
    #[test]
    fn a_job_loops_until_the_completion_marker() {
        let _setup = setup();
        let (snapshot, capture) = run_job(
            vec![
                Reply::Text("first change\n[continue]".to_string()),
                Reply::Text("verified\n[goal-complete]".to_string()),
            ],
            "finish it",
            None,
        );

        assert_eq!(snapshot.status, JobStatus::Completed, "{snapshot:?}");
        assert_eq!(snapshot.iteration, 2, "{snapshot:?}");
        assert_eq!(snapshot.limit, None);
        assert!(
            snapshot
                .latest_result
                .as_deref()
                .is_some_and(contains_completion_marker),
            "{snapshot:?}"
        );
        assert!(snapshot.agent_id.is_some(), "记录 id 必须留痕");

        // 第 1 轮从父级分叉、无 resume；第 2 轮 resume 同一个子会话
        let resumes = capture.resumes.lock().unwrap().clone();
        assert_eq!(resumes.len(), 2, "两轮派发");
        assert_eq!(resumes[0], None);
        assert_eq!(resumes[1].as_deref(), Some("/tmp/autoresearch-child.jsonl"));

        let inherits = capture.inherits.lock().unwrap().clone();
        assert_eq!(inherits, vec![true, false], "只有首轮 fork 父级上下文");

        let prompts = capture.prompts.lock().unwrap().clone();
        assert_eq!(prompts.len(), 2, "{prompts:?}");
        assert!(prompts[0].contains("iteration 1 for this goal"));
        assert!(prompts[1].contains("iteration 2 for this goal"));
    }

    /// 到 `--iterations` 上限即收工（即使模型一直说 `[continue]`），且轮次被 clamp。
    #[test]
    fn a_job_stops_at_the_iteration_limit() {
        let _setup = setup();
        let (snapshot, capture) = run_job(
            vec![
                Reply::Text("[continue]".to_string()),
                Reply::Text("[continue]".to_string()),
            ],
            "keep going",
            Some(2),
        );

        assert_eq!(snapshot.status, JobStatus::Completed, "{snapshot:?}");
        assert_eq!(snapshot.iteration, 2, "{snapshot:?}");
        assert_eq!(snapshot.limit, Some(2));
        assert_eq!(capture.prompts.lock().unwrap().len(), 2, "到上限就不再派发");
    }

    /// `--iterations` 被 clamp 到 [`MAX_ITERATIONS`]（与 kiss 同值）。
    #[test]
    fn iterations_are_clamped_to_the_maximum() {
        let _setup = setup();
        let (snapshot, _) = run_job(
            vec![Reply::Text("[goal-complete]".to_string())],
            "x",
            Some(9_999),
        );
        assert_eq!(snapshot.limit, Some(MAX_ITERATIONS));
    }

    /// 子代理出错 → 作业落 `Failed` 并留下原因，不会继续下一轮。
    #[test]
    fn a_job_fails_when_the_child_errors() {
        let _setup = setup();
        let (snapshot, capture) = run_job(
            vec![Reply::Fail("provider exploded".to_string())],
            "boom",
            None,
        );

        assert_eq!(snapshot.status, JobStatus::Failed, "{snapshot:?}");
        assert_eq!(snapshot.iteration, 1);
        assert!(
            snapshot
                .error
                .as_deref()
                .is_some_and(|e| e.contains("exploded")),
            "{snapshot:?}"
        );
        assert_eq!(capture.prompts.lock().unwrap().len(), 1, "失败即收工");
    }

    /// 无执行上下文时报错而不是静默丢作业（`/autoresearch` 在首个回合前被调）。
    #[test]
    fn start_without_an_execution_context_reports_an_error() {
        let _lock = super::manager::test_lock();
        fleet::forget_ctx_for_test();
        let error = start("do something", None).unwrap_err();
        assert!(error.contains("no execution context"), "{error}");
    }

    /// 停止运行中的作业：置停止信号并掐掉那一轮，作业落 `Stopped`。
    #[test]
    fn stop_ends_a_running_job() {
        let _setup = setup();
        let (ctx, _capture) = script_ctx(vec![Reply::Hold]);
        let runtime = rt();
        let snapshot = runtime.block_on(async move {
            let id = start_with(ctx, tokio::runtime::Handle::current(), "hang", None);
            // 等它真跑起来（派发出去的那一轮已在注册表里）
            let deadline = Instant::now() + Duration::from_secs(5);
            while snapshot(id).is_none_or(|s| s.iteration == 0) {
                assert!(Instant::now() < deadline, "作业没有开始");
                tokio::time::sleep(Duration::from_millis(5)).await;
            }

            stop(id).expect("停止运行中的作业");
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let current = snapshot(id).expect("作业还在表里");
                if current.status.is_finished() {
                    return current;
                }
                assert!(Instant::now() < deadline, "停止没有生效");
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });

        assert_eq!(snapshot.status, JobStatus::Stopped, "{snapshot:?}");
        assert!(
            stop(snapshot.id).is_err(),
            "已终结的作业不能再停（应报错而不是重放）"
        );

        let (notes, _) = drain_ui();
        assert!(notes.is_empty(), "用户主动停止不该再弹通知：{notes:?}");
    }

    /// 终结时投一条**人向**通知（`Completed` → Success，`Failed` → Warning 带原因），
    /// 且**不**投 `Continuation`——作业与主回合解耦，结果不进主对话。
    #[test]
    fn settled_jobs_notify_the_user_without_touching_the_conversation() {
        let _setup = setup();
        let (done, _) = run_job(
            vec![Reply::Text("[goal-complete]".to_string())],
            "ship it",
            None,
        );

        let (notes, continuation) = drain_settled_ui();
        assert!(!continuation, "完成通知不该往主会话注入消息");
        assert_eq!(notes.len(), 1, "一次终结只弹一条：{notes:?}");
        let (text, level) = &notes[0];
        assert_eq!(*level, UiNotifyLevel::Success, "{text}");
        assert!(
            text.contains(&format!("autoresearch job #{} completed · 1 ·", done.id)),
            "{text}"
        );
        assert!(text.contains("ship it"), "{text}");
        assert!(text.contains("/agents result"), "{text}");

        let (failed, _) = run_job(
            vec![Reply::Fail("provider exploded".to_string())],
            "explode",
            Some(3),
        );

        let (notes, continuation) = drain_settled_ui();
        assert!(!continuation, "失败通知同样不该注入消息");
        assert_eq!(notes.len(), 1, "{notes:?}");
        let (text, level) = &notes[0];
        assert_eq!(*level, UiNotifyLevel::Warning, "{text}");
        assert!(
            text.contains(&format!("autoresearch job #{} failed · 1/3", failed.id)),
            "{text}"
        );
        assert!(text.contains("provider exploded"), "{text}");
    }

    /// `cancel_all` 只动未终结的作业：已完成的历史留着（`/new` 后列表还在）。
    #[test]
    fn cancel_all_leaves_finished_jobs_alone() {
        let _setup = setup();
        let (done, _) = run_job(
            vec![Reply::Text("[goal-complete]".to_string())],
            "done",
            None,
        );
        assert_eq!(done.status, JobStatus::Completed);

        cancel_all();
        let after = snapshot(done.id).expect("已终结作业仍在表里");
        assert_eq!(
            after.status,
            JobStatus::Completed,
            "已终结的不该被改成 stopped"
        );
        assert!(!has_live(), "没有未终结作业");
    }

    /// dock 段只列未终结作业；作业终结后该段消失（不占位）。
    #[test]
    fn dock_lists_only_unfinished_jobs() {
        let _setup = setup();
        assert!(dock_lines().is_empty(), "没有作业时不占位");

        let (ctx, _capture) = script_ctx(vec![Reply::Hold]);
        let runtime = rt();
        let id = runtime.block_on(async {
            let id = start_with(ctx, tokio::runtime::Handle::current(), "watch me", None);
            let deadline = Instant::now() + Duration::from_secs(5);
            while !has_live() {
                assert!(Instant::now() < deadline, "作业没有开始");
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            id
        });

        let text: Vec<String> = dock_lines()
            .iter()
            .map(|line| line.iter().map(|span| span.text.as_str()).collect())
            .collect();
        assert!(
            text.iter().any(|line| line.contains("Autoresearch")),
            "{text:?}"
        );
        assert!(
            text.iter().any(|line| line.contains(&format!("#{id}"))),
            "{text:?}"
        );
        assert!(
            text.iter().any(|line| line.contains("watch me")),
            "{text:?}"
        );
        assert!(text.iter().any(|line| line.contains("1 job")), "{text:?}");

        stop(id).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while has_live() {
            assert!(Instant::now() < deadline, "停止没有生效");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(dock_lines().is_empty(), "终结后 dock 段消失");
    }

    /// 列表通知：无作业时给一行说明，有作业时逐条列出（含失败原因）。
    #[test]
    fn list_spans_report_jobs_and_failures() {
        let _setup = setup();
        let empty: String = list_spans().iter().map(|s| s.text.as_str()).collect();
        assert!(empty.contains("No autoresearch jobs"), "{empty}");

        let (snapshot, _) = run_job(vec![Reply::Fail("nope".to_string())], "explode", Some(3));
        assert_eq!(snapshot.status, JobStatus::Failed);

        let text: String = list_spans().iter().map(|s| s.text.as_str()).collect();
        assert!(text.contains("Autoresearch · 0 active · 1 total"), "{text}");
        assert!(
            text.contains(&format!("#{} failed · 1/3", snapshot.id)),
            "{text}"
        );
        assert!(text.contains("error: nope"), "{text}");
        assert!(text.contains("/autoresearch stop <id>"), "{text}");
    }
}
