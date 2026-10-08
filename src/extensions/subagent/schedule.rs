//! 定时子代理：存储 + 调度器 + 列表视图
//!
//! ## 形状
//!
//! 定时任务**不**由定时器直接跑一份新代码：它到点走的是同一条 `manager::dispatch` 路径，
//! 所以"后台运行 + 完成通知 + 卡片 + 记录"全都不用另写。
//!
//! ## 会话作用域
//!
//! 存储按会话分：`<agent_dir>/extensions/schedules/<session>.json`（会话文件的 stem；没有落盘文件的
//! 会话用 `default`）。`/new` 起一份空的，`/resume` 读回旧的。
//!
//! **空表不落地**：最后一个任务消失时文件直接删除（缺失文件读回来同样是空，语义与
//! 留一个 `{"jobs": []}` 等价，但不会在 `schedules/` 下堆垃圾）。落盘用唯一临时文件名 +
//! state 锁内取快照，避免扫描任务与任务收尾并发写盘互相截断。**坏文件不静默清空**：
//! 读到解析失败时先把原文件挪成 `.corrupt` 留证据，再以空表继续。
//!
//! ## 为什么到点还要"绕过并发上限"
//!
//! 每 5 分钟一次的活儿不该排在 4 个长任务后面（那会让它漂到几十分钟后，周期彻底失真）。
//! 所以定时 spawn 带 `bypass_queue`：不占后台额度、也不排队——**配额是给人的突发行为用的，不是给墙钟用的**。
//!
//! ## 定时器
//!
//! 一个 1 秒的 tick 扫描"到点的 enabled 任务"（而不是每个任务一个 `sleep`）：
//! 变更（取消/新增/一次性自动关停）都在同一张表里，扫描不会漏也不会重复武装；
//! 秒级粒度与上游 `setInterval/setTimeout` 的实际精度一致。

pub(crate) mod parse;

#[cfg(test)]
mod tests;

use super::{
    super::util::truncate_chars,
    agent_types, events, fleet, manager,
    model::ModelSource,
    session,
    types::{self, AgentStatus, AgentType, SpawnRequest},
    util_notify,
};
use crate::{
    core::{
        extensions::{DockLine, DockSpan, OverlaySize, OverlayView, ToolExecCtx, UiNotifyLevel},
        settings_manager::agent_dir,
    },
    utils::{
        glyphs::{DEF_ARROW_UP_DOWN, DEF_DONE, DEF_FAILED, DEF_SELECTED_MARK},
        time::{now_ms, rel_time, system_to_ms},
    },
};
use parse::ScheduleKind;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

/// 临时文件序号：临时名必须唯一，否则进程内多线程（扫描任务 vs 任务收尾）或
/// 多进程共用同一个 store 时会同时写同一个 tmp，rename 出去的就是半截 JSON。
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// 读盘结果：区分「无文件」「正常读出任务」「文件损坏」三种情形。
enum StoreRead {
    /// 没有文件
    Missing,
    /// 读到任务
    Jobs(Vec<Job>),
    /// 文件损坏——把损坏当成空表写回去就意味着任务全丢。
    Corrupt,
}

/// 一个定时任务
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Job {
    /// 任务 id（`job_…`；列表展示与停止都用它）。
    pub id: String,
    /// 显示名（= 创建时的 `description`，上游同此）
    pub name: String,
    /// 创建时的简述。
    pub description: String,
    /// 用户写的调度串（`+10m` 规范化成时间戳，其余原样）
    pub schedule: String,
    /// 调度类型（`once` / `interval` / `cron`；见 [`ScheduleKind`]）
    #[serde(rename = "scheduleType")]
    pub schedule_type: String,
    /// interval 类型的周期毫秒数（其余类型为 None）
    #[serde(rename = "intervalMs", skip_serializing_if = "Option::is_none")]
    pub interval_ms: Option<u64>,
    /// 启动时的 agent 类型名（**到点时重新解析**：用户可能改了文件、或类型被删了）
    pub subagent_type: String,
    /// 到点派发的任务正文。
    pub prompt: String,
    /// 型号覆盖（None = 用类型钉死的或继承的）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// 思考档位覆盖。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    /// 最大轮数覆盖。
    #[serde(rename = "maxTurns", skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u32>,
    /// 是否参与扫描（一次性任务跑完自动置 false）。
    pub enabled: bool,
    /// 创建时间（epoch ms）。
    #[serde(rename = "createdAtMs")]
    pub created_at_ms: u64,
    /// 最近一次触发时间（epoch ms；从未跑过为 None）。
    #[serde(rename = "lastRunMs", skip_serializing_if = "Option::is_none")]
    pub last_run_ms: Option<u64>,
    /// 最近一次运行的状态（`running` / `success` / `error`；见 [`Job::icon`]）。
    #[serde(rename = "lastStatus", skip_serializing_if = "Option::is_none")]
    pub last_status: Option<String>,
    /// 下一次触发时间（epoch ms；`recompute_next` 维护，已关停/过期时为 None）。
    #[serde(rename = "nextRunMs", skip_serializing_if = "Option::is_none")]
    pub next_run_ms: Option<u64>,
    /// 累计成功进入终态的触发次数（列表展示用）。
    #[serde(rename = "runCount", default)]
    pub run_count: u64,
}

impl Job {
    /// 解析 `schedule_type` 得到调度类型；无法识别的类型返回 `None`（该任务不会被重算 / 触发）。
    fn kind(&self) -> Option<ScheduleKind> {
        ScheduleKind::parse(&self.schedule_type)
    }

    /// 重算下一次触发（按本地时间/cron 语义）。
    fn recompute_next(&mut self) {
        let Some(kind) = self.kind() else {
            self.next_run_ms = None;
            return;
        };
        self.next_run_ms = parse::next_run_after(
            kind,
            &self.schedule,
            self.interval_ms,
            self.last_run_ms,
            std::time::SystemTime::now(),
        )
        .map(system_to_ms);

        // 一次性任务跑过就关掉
        if kind == ScheduleKind::Once && self.last_run_ms.is_some() {
            self.enabled = false;
        }
    }

    /// 一行状态图标
    pub(crate) fn icon(&self) -> &'static str {
        if !self.enabled {
            DEF_FAILED
        } else if self.last_status.as_deref() == Some("error") {
            "!"
        } else if self.last_status.as_deref() == Some("running") {
            "⋯"
        } else {
            DEF_DONE
        }
    }

    /// 列表行的（主题色键, 缺省色）：已关停取 muted、上次出错取 error，否则 success。
    pub(crate) fn color(&self) -> (&'static str, &'static str) {
        if !self.enabled {
            ("muted", "#999999")
        } else if self.last_status.as_deref() == Some("error") {
            ("error", "#cc6666")
        } else {
            ("success", "#b5bd68")
        }
    }
}

/// 新建任务的输入
pub(crate) struct NewJob {
    /// 显示名（通常取 `description`）。
    pub name: String,
    /// 简述（也用作任务名）。
    pub description: String,
    /// 调度串（cron / interval / 一次性）。
    pub schedule: String,
    /// 类型名（到点时重新解析）。
    pub subagent_type: String,
    /// 任务正文。
    pub prompt: String,
    /// 型号覆盖（None = 类型钉死或继承）。
    pub model: Option<String>,
    /// 思考档位覆盖。
    pub thinking: Option<String>,
    /// 最大轮数覆盖。
    pub max_turns: Option<u32>,
}

/// 进程级定时任务表：当前会话的 jobs、存储路径与 ticker 代际。
#[derive(Default)]
struct State {
    /// 全部任务（按创建序；持久化就是这个数组）。
    jobs: Vec<Job>,
    /// 已经绑定了当前会话的 store（`sync_session` 之后为 true）
    active: bool,
    /// 存储文件（会话切换时换一个）
    path: Option<PathBuf>,
    /// ticker 的"代"：每次绑定会话 +1；旧任务的代对不上就自己退出，
    /// 新会话因此不会被一个还没醒过来的旧任务挡住（`ticking` 那种布尔会）
    ticker_gen: u64,
}

/// 进程级任务表
fn state() -> &'static Mutex<State> {
    /// 进程内唯一的定时任务表实例，扫描与增删改都经它串行化。
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(State::default()))
}

/// 取全局任务表锁并执行 `f` 返回其结果；所有读写任务表的路径都经此串行化。
fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> R {
    let mut st = state().lock().unwrap_or_else(|e| e.into_inner());
    f(&mut st)
}

/// 存储路径：`<agent_dir>/extensions/schedules/<session>.json`。
fn store_path() -> PathBuf {
    let key = session::session_key().unwrap_or_else(|| "default".to_string());
    let key: String = key
        .chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | ':') {
                '-'
            } else {
                c
            }
        })
        .collect();

    agent_dir()
        .join("extensions")
        .join("schedules")
        .join(format!("{key}.json"))
}

/// 载入当前会话的任务并武装（`on_session_start` / `on_session_switched` 调用）。
pub(crate) fn sync_session() {
    let path = store_path();
    let (jobs, writable) = load_store(&path);

    with_state(|st| {
        st.jobs = jobs;
        st.active = true;
        // 坏文件挪不走时放弃写盘（path=None）：宁可这次不落盘，也不能覆盖掉原文件
        st.path = writable.then(|| path.clone());
    });

    // 重算下一次；**过期的一次性任务**标记错误并关停（不静默丢），
    // 其余的交给 ticker（周期/cron 的"到点"判断只有一处：`tick` 里的 `next_run_ms <= now`）
    let mut missed: Vec<String> = Vec::new();
    with_state(|st| {
        for job in st.jobs.iter_mut() {
            job.recompute_next();
            if job.kind() == Some(ScheduleKind::Once) && job.next_run_ms.is_none() && job.enabled {
                job.enabled = false;
                job.last_status = Some("error".to_string());
                missed.push(job.name.clone());
            }
        }
        st.ticker_gen += 1;
    });

    for name in missed {
        util_notify(
            &format!("scheduled job {name:?} was missed (its time has passed); it is now disabled"),
            UiNotifyLevel::Warning,
        );
    }

    save_store();
    start_ticker();
    events::scheduler_ready(list().len());
}

/// 清空（会话切换/扩展禁用）：停止扫描、忘掉任务（文件留着，`/resume` 时读回）。
pub(crate) fn reset() {
    with_state(|st| {
        st.jobs.clear();
        st.active = false;
        st.path = None;
        st.ticker_gen += 1; // 换代：正在睡的扫描任务醒来后自己退出
    });
}

/// 当前会话是否绑定了 store（菜单据此报"这个会话里调度器还没起来"）。
pub(crate) fn is_active() -> bool {
    with_state(|st| st.active)
}

/// 全部任务（按创建时间）。
pub(crate) fn list() -> Vec<Job> {
    with_state(|st| st.jobs.clone())
}

/// 按 id 取任务副本；不存在返回 `None`。
pub(crate) fn get(id: &str) -> Option<Job> {
    with_state(|st| st.jobs.iter().find(|j| j.id == id).cloned())
}

/// 新建：校验调度串、查重名、落盘、武装。
pub(crate) fn add(input: NewJob) -> Result<Job, String> {
    let detected = parse::detect(&input.schedule)?;
    if !is_active() {
        return Err("Scheduler is not active in this session yet.".to_string());
    }

    if with_state(|st| st.jobs.iter().any(|j| j.name == input.name)) {
        return Err(format!(
            "A scheduled job named {:?} already exists.",
            input.name
        ));
    }

    let mut job = Job {
        id: new_job_id(),
        name: input.name,
        description: input.description,
        schedule: detected.normalized,
        schedule_type: detected.kind.as_str().to_string(),
        interval_ms: detected.interval_ms,
        subagent_type: input.subagent_type,
        prompt: input.prompt,
        model: input.model,
        thinking: input.thinking,
        max_turns: input.max_turns,
        enabled: true,
        created_at_ms: now_ms(),
        last_run_ms: None,
        last_status: None,
        next_run_ms: None,
        run_count: 0,
    };
    job.recompute_next();

    with_state(|st| st.jobs.push(job.clone()));
    save_store();
    start_ticker();
    emit_job("added", &job);
    Ok(job)
}

/// 取消一个任务；不存在返回 false。
pub(crate) fn cancel(id: &str) -> bool {
    let removed = with_state(|st| {
        let before = st.jobs.len();
        st.jobs.retain(|j| j.id != id);
        before != st.jobs.len()
    });

    if removed {
        save_store();
        super::events::schedule_changed(serde_json::json!({
            "type": "removed",
            "jobId": id,
        }));
    }
    removed
}

/// 一个任务的"下一次触发"（epoch ms）。
pub(crate) fn next_run(id: &str) -> Option<u64> {
    get(id).and_then(|j| j.next_run_ms)
}

/// 生成 `job_` 前缀的 12 位十六进制 id（毫秒时间戳混合当前任务数，同毫秒建多个也不撞）。
// 与 run id 同风格：前缀 + 12 位十六进制（时间 + 任务数），同一毫秒内建多个也不撞
fn new_job_id() -> String {
    let n = with_state(|st| st.jobs.len() as u64);
    format!("job_{:012x}", (now_ms() << 8 ^ n) & 0xffff_ffff_ffff)
}

/// 从磁盘读任务表：文件缺失 / JSON 解析失败 / 字段形状不符分别归为 [`StoreRead::Missing`] / [`StoreRead::Corrupt`]，
/// 成功则返回 [`StoreRead::Jobs`]。
fn read_store(path: &Path) -> StoreRead {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return StoreRead::Missing,
        Err(_) => return StoreRead::Corrupt,
    };

    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return StoreRead::Corrupt;
    };

    let items = value.get("jobs").cloned().unwrap_or(value);
    match serde_json::from_value::<Vec<Job>>(items) {
        Ok(jobs) => StoreRead::Jobs(jobs),
        Err(_) => StoreRead::Corrupt,
    }
}

/// 读当前会话的 store，返回 `(任务, 是否允许写回该路径)`。
///
/// 损坏时把原文件挪成 `<name>.json.<ts>.corrupt`（保留证据）再以空表继续；
/// 挪不动则返回 false，调用方必须放弃写盘——不能拿空表覆盖掉读不懂的文件。
fn load_store(path: &Path) -> (Vec<Job>, bool) {
    match read_store(path) {
        StoreRead::Jobs(jobs) => (jobs, true),
        StoreRead::Missing => (Vec::new(), true),
        StoreRead::Corrupt => match quarantine(path) {
            Some(moved) => {
                util_notify(
                    &format!(
                        "schedules file {} is unreadable; moved it to {} and starting empty",
                        path.display(),
                        moved.display()
                    ),
                    UiNotifyLevel::Warning,
                );
                (Vec::new(), true)
            }
            None => {
                util_notify(
                    &format!(
                        "schedules file {} is unreadable and could not be moved aside; leaving it untouched",
                        path.display()
                    ),
                    UiNotifyLevel::Warning,
                );
                (Vec::new(), false)
            }
        },
    }
}

/// 坏文件挪到一旁（`<name>.json.<ts>.corrupt`），保留证据而不是清空。
fn quarantine(path: &Path) -> Option<PathBuf> {
    let target = path.with_extension(format!("json.{}.corrupt", now_ms()));
    std::fs::rename(path, &target).ok().map(|_| target)
}

/// 把当前会话的任务表写回磁盘。
///
/// 在 state 锁内取快照再落盘：`tick` 的扫描任务与每个任务的收尾（`finish`/`mark_error`）
/// 跑在多线程 runtime 上，锁内快照保证写出去的是最新状态，也避免两笔写互相插队。
fn save_store() {
    let st = state().lock().unwrap_or_else(|e| e.into_inner());
    let Some(path) = st.path.clone() else {
        return;
    };

    persist(&path, &st.jobs);
}

/// 落盘一个任务表：空表删文件（"空不落地"），非空表原子写。
fn persist(path: &Path, jobs: &[Job]) {
    // 空 store：删掉文件。缺失文件读回来同样是空，
    // `/resume` 不会读回旧任务，也不会在 schedules/ 下堆一地 `{"jobs": []}`。
    if jobs.is_empty() {
        if path.exists() {
            _ = std::fs::remove_file(path);
        }
        return;
    }

    if let Some(parent) = path.parent()
        && std::fs::create_dir_all(parent).is_err()
    {
        return;
    }
    let body = serde_json::json!({ "jobs": jobs });
    let Ok(text) = serde_json::to_string_pretty(&body) else {
        return;
    };

    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("json.tmp.{}.{seq}", std::process::id()));
    if std::fs::write(&tmp, format!("{text}\n")).is_err() {
        _ = std::fs::remove_file(&tmp);
        return;
    }

    if std::fs::rename(&tmp, path).is_err() {
        _ = std::fs::remove_file(&tmp);
    }
}

/// 启动 1 秒 tick 的扫描任务。
///
/// 用"代"而不是布尔：会话切换时旧任务可能还在睡（最多 1 秒），布尔会让新会话永远起不来扫描。
/// 旧任务发现自己的代过期就退出，新任务照常起来。
fn start_ticker() {
    let generation = with_state(|st| st.ticker_gen);
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return; // 没有运行时（同步上下文）→ 下次 `sync_session` 再试
    };

    handle.spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let current = with_state(|st| st.ticker_gen);
            if current != generation || !is_active() {
                return;
            }
            tick();
        }
    });
}

/// 一次扫描：到点的都触发（触发是**fire-and-forget**，别的任务照常到点）。
fn tick() {
    let now = now_ms();
    let due: Vec<Job> = with_state(|st| {
        st.jobs
            .iter()
            .filter(|j| j.enabled && j.next_run_ms.is_some_and(|n| n <= now))
            .cloned()
            .collect()
    });

    for job in due {
        // 先记状态再派生：派发是异步的，重复 tick 不该重复触发
        with_state(|st| {
            if let Some(j) = st.jobs.iter_mut().find(|j| j.id == job.id) {
                j.last_status = Some("running".to_string());
                j.last_run_ms = Some(now);
                j.recompute_next();
            }
        });

        save_store();
        fire(job);
    }
}

/// 触发一个任务：解析类型 → 后台 spawn（不排队）→ 完成时记账。
fn fire(job: Job) {
    let Some((ctx, handle)) = fleet::cached_ctx() else {
        mark_error(&job.id, "scheduler has no execution context yet");
        return;
    };
    let roster = agent_types::discover(&ctx.cwd);
    let ty = match agent_types::resolve(&roster.types, &job.subagent_type) {
        Ok(ty) => ty.clone(),
        Err(e) => {
            mark_error(
                &job.id,
                &format!("agent type {:?}: {e:?}", job.subagent_type),
            );
            return;
        }
    };
    let req = request_for_job(&job, &ty);
    handle.spawn(async move {
        run_job(ctx, ty, req, job.id, job.name).await;
    });
}

/// 定时任务的后台运行体：派发 → 等终态 → 记账。
async fn run_job(ctx: ToolExecCtx, ty: AgentType, req: SpawnRequest, job_id: String, name: String) {
    match manager::dispatch(&ctx, &ty, req).await {
        Ok(dispatched) => {
            // 完成记账：等这个记录到终态（后台运行的完成通知走既有路径）
            let id = dispatched.id.clone();
            events::schedule_changed(serde_json::json!({
                "type": "fired",
                "jobId": job_id,
                "agentId": id,
                "name": name,
            }));
            finish(&job_id, !await_terminal(&id).await);
        }
        Err(e) => {
            util_notify(
                &format!("scheduled job {name:?} could not start: {e}"),
                UiNotifyLevel::Warning,
            );
            mark_error(&job_id, &e);
        }
    }
}

/// 等一个记录到终态，返回它是否属于失败终态（`Error`/`Aborted`/`Stopped`）。
async fn await_terminal(id: &str) -> bool {
    let done = manager::record(id).and_then(|r| r.done_rx.clone());
    if let Some(mut done) = done {
        // 通道在收尾时置 true；等不到也只是少记一次状态
        while !*done.borrow() {
            if done.changed().await.is_err() {
                break;
            }
        }
    }

    manager::record(id).is_none_or(|r| {
        matches!(
            r.status,
            AgentStatus::Error | AgentStatus::Aborted | AgentStatus::Stopped
        )
    })
}

/// 由任务与类型构造 spawn 请求：定时任务固定后台、且不占后台并发额度（见模块文档）。
fn request_for_job(job: &Job, ty: &AgentType) -> SpawnRequest {
    let mut req = SpawnRequest::from_type(
        ty,
        &manager::config(),
        job.prompt.clone(),
        job.description.clone(),
    );
    req.model = job.model.clone().or_else(|| ty.model.clone());
    req.model_source = if job.model.is_some() {
        ModelSource::Caller
    } else {
        types::model_source_from_type(ty)
    };
    req.thinking = job.thinking.clone().or_else(|| ty.thinking.clone());
    req.max_turns = job.max_turns.or(ty.max_turns);
    req.run_in_background = true;
    req.bypass_queue = true;
    req.persist_session = ty.persist_session.unwrap_or(true);
    req
}

/// 任务收尾记账：按 `success` 写 `last_status`、累加运行次数并重算下次触发，随后落盘并广播更新。
fn finish(id: &str, success: bool) {
    let job = with_state(|st| {
        let j = st.jobs.iter_mut().find(|j| j.id == id)?;
        j.last_status = Some(if success { "success" } else { "error" }.to_string());
        j.run_count += 1;
        j.recompute_next();
        Some(j.clone())
    });

    save_store();

    if let Some(job) = job {
        emit_job("updated", &job);
    }
}

/// 把一个任务对象发到 `subagents:scheduled`（`{type, job}`）。
fn emit_job(kind: &str, job: &Job) {
    let job = serde_json::to_value(job).unwrap_or(Value::Null);
    events::schedule_changed(serde_json::json!({ "type": kind, "job": job }));
}

/// 标记任务失败：写 `last_status = "error"` 并重算下次触发，落盘后广播错误事件与提示。
fn mark_error(id: &str, reason: &str) {
    with_state(|st| {
        if let Some(j) = st.jobs.iter_mut().find(|j| j.id == id) {
            j.last_status = Some("error".to_string());
            j.recompute_next();
        }
    });
    save_store();
    events::schedule_changed(serde_json::json!({
        "type": "error",
        "jobId": id,
        "error": reason,
    }));
    util_notify(
        &format!("scheduled job failed: {reason}"),
        UiNotifyLevel::Warning,
    );
}

/// 列表覆盖层（`/agents schedules`）：一行一个任务，选中行在底部给出详情。
pub(crate) fn view(selected: usize) -> OverlayView {
    let jobs = list();
    let selected = if jobs.is_empty() {
        0
    } else {
        selected.min(jobs.len() - 1)
    };

    let mut lines: Vec<DockLine> = Vec::new();
    if jobs.is_empty() {
        lines.push(vec![DockSpan::new(
            "dim",
            "#666666",
            "no scheduled jobs in this session",
        )]);
    }

    for (i, j) in jobs.iter().enumerate() {
        let (key, fallback) = j.color();
        let marker = if i == selected {
            DEF_SELECTED_MARK
        } else {
            "  "
        };
        lines.push(vec![
            DockSpan::new(
                "muted",
                "#999999",
                if i == selected {
                    DEF_SELECTED_MARK
                } else {
                    "  "
                },
            ),
            DockSpan::new(key, fallback, format!("{} ", j.icon())),
            DockSpan::new("accent", "#8abeb7", format!("{:<20}", j.name)),
            DockSpan::new("dim", "#666666", format!("{:<16}", j.schedule)),
            DockSpan::new("dim", "#666666", format!("[{}]", j.subagent_type)),
            DockSpan::new(
                "dim",
                "#666666",
                format!("  next {}", rel_time(j.next_run_ms)),
            ),
            DockSpan::new(
                "dim",
                "#666666",
                format!("  last {}", rel_time(j.last_run_ms)),
            ),
            DockSpan::new("dim", "#666666", format!("  runs {}", j.run_count)),
            DockSpan::new("dim", "#666666", marker),
        ]);
    }

    // 选中任务的详情：取消前要看清它到底会跑什么
    if let Some(j) = jobs.get(selected) {
        lines.push(vec![DockSpan::plain("")]);
        for (k, v) in [
            ("id", j.id.clone()),
            ("schedule", format!("{} ({})", j.schedule, j.schedule_type)),
            ("agent", j.subagent_type.clone()),
            ("prompt", truncate_chars(&j.prompt, 200)),
            (
                "created",
                rel_time(Some(j.created_at_ms)).replace("in ", ""),
            ),
            (
                "last run",
                format!(
                    "{} ({})",
                    rel_time(j.last_run_ms),
                    j.last_status.clone().unwrap_or_else(|| "—".to_string())
                ),
            ),
        ] {
            lines.push(vec![
                DockSpan::new("muted", "#999999", format!("  {k}: ")),
                DockSpan::new("dim", "#666666", v),
            ]);
        }
    }
    OverlayView {
        title: format!("Scheduled jobs · {}", jobs.len()),
        lines,
        footer: vec![
            (DEF_ARROW_UP_DOWN.to_string(), "select".to_string()),
            ("d".to_string(), "cancel job".to_string()),
            ("Esc".to_string(), "close".to_string()),
        ],
        input: None,
        editor: None,
        size: OverlaySize::Panel { max_rows: 18 },
        header: Vec::new(),
        messages: Vec::new(),
        selected: None,
        input_focus: false,
    }
}
