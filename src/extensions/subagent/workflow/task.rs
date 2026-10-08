//! 后台工作流运行注册表。
//!
//! `SubagentWorkflow` 的调用**立刻返回 task id**，运行在它之后继续，
//! 所以一次运行的状态不能活在工具调用的闭包里：完成通知、运行列表覆盖层、dock 那行，
//! 都要在 `execute` 返回之后还能读到它。这就是那份记录。
//!
//! 与 [`super::super::manager`] 的关系是**并列**而不是包含：`AgentRecord` 的字段
//! （轮数、工具次数、steer 句柄、转写）对一次运行大半无意义，反过来 run 的字段
//! （脚本、返回值、进度日志）也不属于一个 agent。两个注册表在扩展边界组合
//! （dock 两段、通知走同一个 join 批次），核心互不认识。
//!
//! 进度是 append-only 的（见 [`super::progress`]），所以这里**只持有日志**：
//! 所有计数（done/total、tokens、耗时）都在渲染时由折叠得到，不在写入时累加——
//! 重发同 index 的条目会替换前一条，累加会把它重复计一次。

use super::{
    super::super::subagent::notify,
    bridge::WorkflowControl,
    meta::WorkflowPhaseMeta,
    progress::{self, AgentEntry, DisplayState, GroupStatus, PhaseGroup, RunHeader},
};
use crate::{
    core::extensions::{DockLine, DockSpan, OverlaySize, OverlayView},
    extensions::util::truncate_chars,
    utils::{
        glyphs::{
            DEF_ARROW_UP_DOWN, DEF_DONE, DEF_FAILED, DEF_PAUSED, DEF_RUNNING, DEF_SELECTED_MARK,
            DEF_STOPPED,
        },
        time::{format_duration, now_ms},
    },
};
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};
use strum_macros::IntoStaticStr;

/// 共享的进度日志（run 持有、kernel 往里写、界面每帧读）。
pub(crate) type ProgressLog = Arc<Mutex<Vec<Value>>>;

/// dock 里最多显示几个 run（超出的靠覆盖层看）。
const DOCK_MAX_RUNS: usize = 3;

/// 一次运行的状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr)]
#[strum(serialize_all = "lowercase")]
pub(crate) enum RunStatus {
    /// 正在运行（agent 已启动、尚未全部结束）。
    Running,
    /// 用户按了暂停（不再启动新 agent；在跑的跑完）
    Paused,
    /// 全部 agent 正常跑完。
    Completed,
    /// 运行出错而结束。
    Failed,
    /// 被用户取消或被系统终止。
    Killed,
}

impl RunStatus {
    /// 状态的稳定字符串形式（小写，与 `IntoStaticStr` 一致，用于事件/日志）。
    pub(crate) fn as_str(self) -> &'static str {
        self.into()
    }

    /// 运行是否已结束（`Running`/`Paused` 之外都算终态）。
    pub(crate) fn is_terminal(self) -> bool {
        !matches!(self, RunStatus::Running | RunStatus::Paused)
    }

    /// 状态对应的字形图标。
    pub(crate) fn icon(self) -> &'static str {
        match self {
            RunStatus::Running => DEF_RUNNING,
            RunStatus::Paused => DEF_PAUSED,
            RunStatus::Completed => DEF_DONE,
            RunStatus::Failed => DEF_FAILED,
            RunStatus::Killed => DEF_STOPPED,
        }
    }

    /// 状态配色：`(主题色键, 回退十六进制值)`。
    pub(crate) fn color(self) -> (&'static str, &'static str) {
        match self {
            RunStatus::Running => ("accent", "#8abeb7"),
            RunStatus::Paused => ("warning", "#f0c674"),
            RunStatus::Completed => ("success", "#b5bd68"),
            RunStatus::Failed => ("error", "#cc6666"),
            RunStatus::Killed => ("warning", "#f0c674"),
        }
    }
}

/// 一次运行的记录快照。`progress` 是共享的，快照因此能看到**进行中**的日志。
#[derive(Clone)]
pub(crate) struct WorkflowRun {
    /// 运行 id（`wf_…`）。
    pub id: String,
    /// 工作流名。
    pub name: String,
    /// 用途简述。
    pub description: String,
    /// 运行状态。
    pub status: RunStatus,
    /// 开始时间（epoch ms）。
    pub started_ms: u64,
    /// 结束时间（None = 未结束）。
    pub ended_ms: Option<u64>,
    /// 运行时**已排定**的 agent 数（fan-out 会在它们开跑前先报出规模）。
    pub agent_count: u64,
    /// 脚本 `return` 的值（成功时）。
    pub value: Option<Value>,
    /// 失败原因（成功为 None）。
    pub error: Option<String>,
    /// 共享进度日志（快照因此能看到进行中的日志）。
    pub progress: ProgressLog,
    /// `meta.phases`（阶段归并要用）。
    pub phases: Option<Vec<WorkflowPhaseMeta>>,
    /// 脚本落在磁盘上的位置（`scriptPath`/具名命中，或本次调用自动落盘的那份）；通知信封里告诉模型"改它再跑"。
    pub script_path: Option<String>,
    /// 本次是恢复哪一次运行（`resumeFromRunId`）；通知摘要里说明重放了多少条。
    pub resumed_from: Option<String>,
    /// 取消标志（同时也是 kernel 的 `cancel`）：置位即中断脚本并联动中止子代理。
    pub abort: Arc<AtomicBool>,
    /// 运行的控制面（kernel 在开跑前交回）：检查器用它 pause/skip/retry。
    pub control: Option<WorkflowControl>,
    /// 用户显式请求停止（区分 `killed` 与脚本自身出错）。
    pub stop_requested: bool,
}

impl WorkflowRun {
    /// 已用时（已终结则固定）。
    pub(crate) fn elapsed_ms(&self) -> u64 {
        self.ended_ms
            .unwrap_or_else(now_ms)
            .saturating_sub(self.started_ms)
    }

    /// 当前日志（克隆一份，供纯函数渲染）。
    pub(crate) fn log(&self) -> Vec<Value> {
        self.progress.lock().unwrap().clone()
    }

    /// 折叠后的阶段分组。
    pub(crate) fn groups(&self) -> Vec<PhaseGroup> {
        let entries = progress::parse_entries(&self.log());
        progress::build_phase_groups(&entries, self.phases.as_deref())
    }

    /// 头部计数。
    pub(crate) fn stats(&self) -> progress::Stats {
        progress::stats(&self.log(), self.agent_count)
    }

    /// 全部 agent 的 token 合计
    pub(crate) fn total_tokens(&self) -> u64 {
        self.groups().iter().map(|g| g.tokens).sum()
    }

    /// 模型看到的 `result` 正文
    pub(crate) fn result_text(&self) -> String {
        if let Some(err) = self.error.as_deref() {
            return err.to_string();
        }

        match self.value.as_ref() {
            None => "No output.".to_string(),
            Some(Value::String(s)) => s.clone(),
            Some(v) => serde_json::to_string_pretty(v).unwrap_or_else(|_| "No output.".to_string()),
        }
    }

    /// 标题行（覆盖层与卡片共用）。
    pub(crate) fn header(&self) -> RunHeader {
        let groups = self.groups();
        progress::header(
            &self.name,
            &self.description,
            self.status,
            &groups,
            self.agent_count,
            self.elapsed_ms(),
        )
    }
}

/// 注册表状态（进程级单例，与 `manager` 一致）。
#[derive(Default)]
struct State {
    /// 全部运行（id → 快照）。
    runs: HashMap<String, WorkflowRun>,
    /// 插入顺序（列表按它 + 状态排序）。
    order: Vec<String>,
}

/// 进程级运行注册表。
fn state() -> &'static Mutex<State> {
    /// 工作流运行的进程级注册表单例，保存全部 run 快照与插入顺序。
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(State::default()))
}

/// 加锁访问进程级运行注册表，在锁内以 `&mut State` 执行 `f`（锁中毒时沿用内部状态）。
fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> R {
    let mut st = state().lock().unwrap_or_else(|e| e.into_inner());
    f(&mut st)
}

/// 新 run id：`wf_` + 12 位 hex
pub(crate) fn new_id() -> String {
    let mut b = [0u8; 6];
    if getrandom::fill(&mut b).is_err() {
        let n = now_ms();
        b.copy_from_slice(&n.to_be_bytes()[2..]);
    }
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("wf_{hex}")
}

/// 登记一次新运行（状态 `running`）。返回取消标志，调用方把它交给 kernel 当 `cancel`。
pub(crate) fn register(
    id: &str,
    name: &str,
    description: &str,
    phases: Option<Vec<WorkflowPhaseMeta>>,
    script_path: Option<String>,
    resumed_from: Option<String>,
) -> Arc<AtomicBool> {
    let abort = Arc::new(AtomicBool::new(false));
    let run = WorkflowRun {
        id: id.to_string(),
        name: name.to_string(),
        description: description.to_string(),
        status: RunStatus::Running,
        started_ms: now_ms(),
        ended_ms: None,
        agent_count: 0,
        value: None,
        error: None,
        progress: Arc::new(Mutex::new(Vec::new())),
        phases,
        script_path,
        resumed_from,
        abort: abort.clone(),
        control: None,
        stop_requested: false,
    };

    with_state(|st| {
        st.order.push(id.to_string());
        st.runs.insert(id.to_string(), run);
    });
    abort
}

/// 挂上控制面（kernel 在第一个 agent 开跑前调）。
pub(crate) fn set_control(id: &str, control: WorkflowControl) {
    with_state(|st| {
        if let Some(r) = st.runs.get_mut(id) {
            r.control = Some(control);
        }
    });
}

/// 用户暂停/继续（检查器的 `p`）：只改 run 的**展示状态**，
/// 真正的"不再启动新 agent" 由 kernel 的控制面负责（两者一起才成立）。
pub(crate) fn set_paused(id: &str, paused: bool) {
    with_state(|st| {
        if let Some(r) = st.runs.get_mut(id)
            && !r.status.is_terminal()
        {
            r.status = if paused {
                RunStatus::Paused
            } else {
                RunStatus::Running
            };
        }
    });
}

/// 记录运行时已排定的 agent 数（fan-out 先报规模，界面据此不让总数往上爬）。
pub(crate) fn set_agent_count(id: &str, count: u64) {
    with_state(|st| {
        if let Some(r) = st.runs.get_mut(id) {
            r.agent_count = count;
        }
    });
}

/// 收尾：写状态/返回值/错误/结束时间。
pub(crate) fn finish(id: &str, status: RunStatus, value: Option<Value>, error: Option<String>) {
    with_state(|st| {
        if let Some(r) = st.runs.get_mut(id) {
            r.status = status;
            r.value = value;
            r.error = error;
            r.ended_ms = Some(now_ms());
        }
    });
}

/// 停止一次运行：置取消标志（脚本被中断、子代理联动中止），并标记为用户请求。已终结的 run 返回 `Err`。
pub(crate) fn stop(key: &str) -> Result<(), String> {
    let target = with_state(|st| {
        if st.runs.contains_key(key) {
            return Some(key.to_string());
        }

        st.runs
            .values()
            .find(|r| r.name == key)
            .map(|r| r.id.clone())
    });

    let Some(id) = target else {
        return Err(format!("unknown workflow run: {key}"));
    };

    let settled = with_state(|st| {
        let Some(r) = st.runs.get_mut(&id) else {
            return true;
        };

        if r.status.is_terminal() {
            return true;
        }

        r.stop_requested = true;
        r.abort.store(true, Ordering::Relaxed);
        false
    });

    if settled {
        return Err(format!("workflow run {key} has already finished"));
    }

    Ok(())
}

/// 取一条记录（按 id 或名字）。
pub(crate) fn find(key: &str) -> Option<WorkflowRun> {
    with_state(|st| {
        st.runs
            .get(key)
            .or_else(|| st.runs.values().find(|r| r.name == key))
            .cloned()
    })
}

/// 运行中（含刚登记、还没跑出第一个 agent）的 run 数。
pub(crate) fn running_count() -> usize {
    with_state(|st| st.runs.values().filter(|r| !r.status.is_terminal()).count())
}

/// 列表：运行中在前，其后按结束时间倒序（与 `manager::list` 同一口径）。
pub(crate) fn list() -> Vec<WorkflowRun> {
    with_state(|st| {
        let mut runs: Vec<WorkflowRun> = st
            .order
            .iter()
            .filter_map(|id| st.runs.get(id).cloned())
            .collect();

        runs.sort_by_key(|r| {
            (
                if r.status.is_terminal() { 1 } else { 0 },
                std::cmp::Reverse(r.ended_ms.unwrap_or(r.started_ms)),
            )
        });
        runs
    })
}

/// 是否有运行中的 run（`wants_redraw` 用）。
pub(crate) fn has_live() -> bool {
    running_count() > 0
}

/// 会话切换 / 扩展禁用：中止全部运行并清空注册表
pub(crate) fn reset_all() {
    with_state(|st| {
        for id in st.order.clone() {
            if let Some(r) = st.runs.get_mut(&id) {
                r.stop_requested = true;
                r.abort.store(true, Ordering::Relaxed);
            }
        }

        st.runs.clear();
        st.order.clear();
    });
}

/// dock 的一段：`Workflows · 1 running` + 每个运行中的 run 一行。
///
/// 只列运行中的 run：已终结的由完成卡片与人回顾（覆盖层里仍在）。
pub(crate) fn dock_lines() -> Vec<DockLine> {
    let live: Vec<WorkflowRun> = list()
        .into_iter()
        .filter(|r| !r.status.is_terminal())
        .collect();

    if live.is_empty() {
        return Vec::new();
    }

    let mut out = vec![vec![
        DockSpan::new("accent", "#8abeb7", "Workflows"),
        DockSpan::new("dim", "#666666", format!(" · {} running", live.len())),
    ]];

    for run in live.iter().take(DOCK_MAX_RUNS) {
        out.push(dock_line(run));
    }
    out
}

/// 一个 run 的 dock 行：`◐ find-flaky-tests wf_ab12… · 3/7 agents · 0m42s · Scanning (1/3)`。
fn dock_line(run: &WorkflowRun) -> DockLine {
    let groups = run.groups();
    let stats = run.stats();
    let (key, fallback) = run.status.color();

    let mut spans = vec![
        DockSpan::plain("  "),
        DockSpan::new(key, fallback, format!("{} ", run.status.icon())),
        DockSpan::new("accent", "#8abeb7", run.name.clone()),
        DockSpan::new("dim", "#666666", format!(" {}", run.id)),
        DockSpan::new(
            "dim",
            "#666666",
            format!(
                " · {}/{} agents · {}",
                stats.done,
                stats.total.max(run.agent_count as usize),
                format_duration(run.elapsed_ms())
            ),
        ),
    ];

    let active = progress::active_phase_titles(&groups);

    if !active.is_empty() {
        let label = progress::footer_phase_label(&active, stats.started, groups.len());
        if !label.is_empty() {
            spans.push(DockSpan::new("dim", "#666666", format!(" · {label}")));
        }
    }
    spans
}

/// 运行列表覆盖层：每个 run 一行摘要 + 它折叠后的树；`Enter` 进检查器。
///
/// `selected` 是 `list()` 里的下标（↑↓ 移动）。
pub(crate) fn overlay_view(selected: usize) -> OverlayView {
    let runs = list();
    let live = runs.iter().filter(|r| !r.status.is_terminal()).count();
    let mut lines: Vec<DockLine> = Vec::new();

    if runs.is_empty() {
        lines.push(vec![DockSpan::new(
            "dim",
            "#666666",
            "no workflow runs in this session",
        )]);
    }

    let selected = if runs.is_empty() {
        0
    } else {
        selected.min(runs.len() - 1)
    };

    // 选中 run 的**首行**在 `lines` 里的下标：作为 `OverlayView.selected` 交回 TUI，
    // 由它保证选中项始终在视口内。一个 run 会展开成多行（摘要 + 阶段 + agent），
    // 列表长度很容易超过 `max_rows`；没有这个字段，选中项滚出视口后画面“看起来没动”。
    let mut selected_line: Option<usize> = None;

    for (i, run) in runs.iter().enumerate() {
        if i == selected && selected_line.is_none() {
            selected_line = Some(lines.len());
        }
        lines.extend(run_lines(run, i == selected));
    }

    let mut footer = vec![
        (DEF_ARROW_UP_DOWN.to_string(), "select".to_string()),
        ("Enter".to_string(), "inspect".to_string()),
        ("Esc".to_string(), "close".to_string()),
    ];

    if live > 0 {
        footer.insert(0, ("/agents stop <id>".to_string(), "stop".to_string()));
    }

    OverlayView {
        title: format!("Workflows · {live} running · {} total", runs.len()),
        header: Vec::new(),
        lines,
        messages: Vec::new(),
        footer,
        input: None,
        editor: None,
        size: OverlaySize::Panel { max_rows: 16 },
        selected: selected_line,
        input_focus: false,
    }
}

/// 一个 run 的整段渲染：摘要行 + 阶段分组 + 组内 agent 行。
fn run_lines(run: &WorkflowRun, selected: bool) -> Vec<DockLine> {
    let groups = run.groups();
    let header = run.header();
    let (key, fallback) = run.status.color();

    let mut lines: Vec<DockLine> = vec![vec![
        DockSpan::new(
            if selected { "accent" } else { "muted" },
            if selected { "#8abeb7" } else { "#999999" },
            if selected { DEF_SELECTED_MARK } else { "  " },
        ),
        DockSpan::new(key, fallback, format!("{} ", run.status.icon())),
        DockSpan::new("accent", "#8abeb7", header.name.clone()),
        DockSpan::new("dim", "#666666", format!(" {} · {}", run.id, header.stats)),
    ]];

    if !header.subtext.is_empty() {
        lines.push(vec![DockSpan::new(
            "dim",
            "#666666",
            format!("    {}", truncate_chars(&header.subtext, 96)),
        )]);
    }

    for group in &groups {
        lines.push(group_line(group));
        for agent in &group.agents {
            lines.push(agent_line(agent, run.status.is_terminal()));
        }
    }

    lines
}

/// 阶段组标题行：`    ✓ Scan 3/3 · 1.2k tok · 12s`。
fn group_line(group: &PhaseGroup) -> DockLine {
    let mut spans = vec![DockSpan::plain("    ")];
    let (key, fallback) = group_status_color(group.status);

    spans.push(DockSpan::new(
        key,
        fallback,
        format!("{} ", group.status.icon()),
    ));
    spans.push(DockSpan::plain(group.title.clone()));

    match group.status {
        GroupStatus::NotStarted => {
            spans.push(DockSpan::new("dim", "#666666", " · not started"));
        }
        _ => {
            spans.push(DockSpan::new(
                "dim",
                "#666666",
                format!(" {}/{}", group.done, group.total),
            ));

            if group.tokens > 0 {
                spans.push(DockSpan::new(
                    "dim",
                    "#666666",
                    format!(
                        " · {} tok",
                        crate::extensions::subagent::notify::format_tokens(group.tokens)
                    ),
                ));
            }

            if group.duration_ms > 0 {
                spans.push(DockSpan::new(
                    "dim",
                    "#666666",
                    format!(" · {}", format_duration(group.duration_ms)),
                ));
            }
        }
    }
    spans
}

/// 阶段组状态的配色：`(主题色键, 回退十六进制值)`。
fn group_status_color(status: GroupStatus) -> (&'static str, &'static str) {
    match status {
        GroupStatus::NotStarted => ("muted", "#999999"),
        GroupStatus::Running => ("accent", "#8abeb7"),
        GroupStatus::Done => ("success", "#b5bd68"),
        GroupStatus::Failed => ("error", "#cc6666"),
    }
}

/// 组内一行：`      ✓ scan src/routes/auth.ts · 3.4s`（失败时带上原因）。
fn agent_line(agent: &AgentEntry, run_settled: bool) -> DockLine {
    let state = progress::display_state(agent, !run_settled);
    let (key, fallback) = state.color();
    let mut spans = vec![
        DockSpan::plain("      "),
        DockSpan::new(key, fallback, format!("{} ", state.icon())),
        DockSpan::plain(truncate_chars(&agent.label, 72)),
    ];
    let mut tail: Vec<String> = Vec::new();

    if let Some(ms) =
        agent
            .duration_ms
            .or_else(|| match (agent.started_at, agent.last_progress_at) {
                (Some(s), Some(l)) => Some(l.saturating_sub(s)),
                _ => None,
            })
    {
        tail.push(format_duration(ms));
    }

    if agent.tokens.unwrap_or(0) > 0 {
        tail.push(format!(
            "{} tok",
            notify::format_tokens(agent.tokens.unwrap_or(0))
        ));
    }

    if !tail.is_empty() {
        spans.push(DockSpan::new(
            "dim",
            "#666666",
            format!(" · {}", tail.join(" · ")),
        ));
    }

    if matches!(state, DisplayState::Failed | DisplayState::Blocked)
        && let Some(err) = agent.error.as_deref()
    {
        spans.push(DockSpan::new(
            "error",
            "#cc6666",
            format!(" · {}", truncate_chars(err, 60)),
        ));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn lock() -> crate::extensions::subagent::TestLock {
        super::super::super::test_lock()
    }

    /// 登记一条运行，塞两条 agent 进度，收尾，然后检查渲染。
    #[test]
    fn run_lifecycle_renders_summary_and_tree() {
        let _g = lock();
        reset_all();
        let id = new_id();
        assert!(id.starts_with("wf_"), "{id}");
        assert_eq!(id.len(), 3 + 12);
        let _abort = register(
            &id,
            "find-flaky-tests",
            "one per flaky test",
            None,
            None,
            None,
        );
        set_agent_count(&id, 2);
        let log = find(&id).unwrap().progress.clone();
        log.lock().unwrap().push(json!({
            "type": "workflow_agent", "index": 0, "label": "scan a", "state": "done",
            "queuedAt": 1, "startedAt": 2, "lastProgressAt": 12, "tokens": 500,
            "recordId": "ab12cd34"
        }));
        log.lock().unwrap().push(json!({
            "type": "workflow_agent", "index": 1, "label": "scan b", "state": "progress",
            "queuedAt": 1, "startedAt": 2
        }));

        // 运行中：dock 有一段，覆盖层里两行 agent 里有一个 interrupted 之外的 running
        assert_eq!(running_count(), 1);
        let dock = dock_lines();
        assert_eq!(dock.len(), 2, "header + 一个 run 行");
        let row = dock_text(&dock[1]);
        assert!(row.contains("find-flaky-tests"), "{row}");
        assert!(row.contains("wf_"), "{row}");
        assert!(row.contains("1/2 agents"), "{row}");

        let view = overlay_view(0);
        assert!(view.title.contains("1 running"));
        let text: Vec<String> = view.lines.iter().map(dock_text).collect();
        assert!(text.iter().any(|l| l.contains("Agents")), "{text:?}");
        assert!(text.iter().any(|l| l.contains("scan a")), "{text:?}");
        assert!(text.iter().any(|l| l.contains("1/2")), "{text:?}");

        // 收尾后：dock 段消失；覆盖层里那条仍在，且仍在飞行的行画成 interrupted
        finish(&id, RunStatus::Completed, Some(json!({"ok": true})), None);
        assert!(dock_lines().is_empty());
        assert_eq!(running_count(), 0);
        let view = overlay_view(0);
        assert!(view.title.contains("0 running"), "{}", view.title);
        let text: Vec<String> = view.lines.iter().map(dock_text).collect();
        assert!(text.iter().any(|l| l.contains("■ scan b")), "{text:?}");
        assert!(text.iter().any(|l| l.contains("✓ scan a")), "{text:?}");
        assert_eq!(
            find(&id).unwrap().result_text(),
            serde_json::to_string_pretty(&json!({"ok": true})).unwrap()
        );
        reset_all();
    }

    /// 选中 run 的首行下标要交回 TUI（`OverlayView.selected`），
    /// 否则内容超过视口时选中项滚出画面，上下键“看起来没动”。
    #[test]
    fn overlay_view_exposes_the_selected_run_line() {
        let _g = lock();
        reset_all();
        for i in 0..3 {
            let id = new_id();
            let _abort = register(&id, &format!("w{i}"), "d", None, None, None);
            find(&id).unwrap().progress.lock().unwrap().push(json!({
                "type": "workflow_agent", "index": 0, "label": "a", "state": "done"
            }));
        }

        let v = overlay_view(1);
        let marker = v
            .lines
            .iter()
            .position(|l| dock_text(l).starts_with("▶ "))
            .expect("应有选中 run 的标记行");
        assert_eq!(v.selected, Some(marker), "selected = 选中 run 的首行");
        // 标记行与 `selected` 要指向同一个 run（选中行的下一行是它的内容）
        assert!(v.selected.unwrap() < v.lines.len());
        reset_all();
    }

    #[test]
    fn stop_marks_requested_and_kills_only_live_runs() {
        let _g = lock();
        reset_all();
        let id = new_id();
        let abort = register(&id, "n", "d", None, None, None);
        assert!(!abort.load(Ordering::Relaxed));
        stop(&id).unwrap();
        assert!(abort.load(Ordering::Relaxed), "停止必须置取消标志");
        assert!(find(&id).unwrap().stop_requested);
        // 按名字也能停
        stop("n").unwrap();
        finish(
            &id,
            RunStatus::Killed,
            None,
            Some("workflow aborted".into()),
        );
        let err = stop(&id).unwrap_err();
        assert!(err.contains("already finished"), "{err}");
        let err = stop("wf_nope").unwrap_err();
        assert!(err.contains("unknown workflow run"), "{err}");
        reset_all();
    }

    #[test]
    fn reset_all_aborts_and_clears() {
        let _g = lock();
        reset_all();
        let id = new_id();
        let abort = register(&id, "n", "d", None, None, None);
        reset_all();
        assert!(abort.load(Ordering::Relaxed), "会话切换必须中断运行");
        assert!(list().is_empty());
        assert!(!has_live());
    }

    #[test]
    fn dock_lines_cap_the_number_of_runs() {
        let _g = lock();
        reset_all();
        let mut aborts = Vec::new();
        for i in 0..5 {
            let id = new_id();
            aborts.push(register(&id, &format!("w{i}"), "d", None, None, None));
        }
        let dock = dock_lines();
        assert_eq!(dock.len(), 1 + DOCK_MAX_RUNS, "header + 上限内的 run 行");
        reset_all();
    }

    /// `DockLine` → 纯文本（断言用）。
    fn dock_text(line: &DockLine) -> String {
        line.iter().map(|s| s.text.as_str()).collect()
    }

    /// strum 派生的规范名：卡片载荷与通知信封里的那个字符串。
    #[test]
    fn run_status_strum_conversions() {
        assert_eq!(RunStatus::Running.as_str(), "running");
        assert_eq!(RunStatus::Paused.as_str(), "paused");
        assert_eq!(RunStatus::Completed.as_str(), "completed");
        assert_eq!(RunStatus::Failed.as_str(), "failed");
        assert_eq!(RunStatus::Killed.as_str(), "killed");
    }
}
