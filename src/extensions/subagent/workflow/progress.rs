//! 工作流进度模型（上游 `workflow/progress.ts` 的移植）。
//!
//! 进度是**append-only 的事件日志**，不是树：agent 条目按 `index` 键 last-write-wins，
//! 所以"更新一个正在跑的 agent"是**再追加一条同 index 的条目**，而不是就地改。
//! 所有视图（完成卡片、运行列表覆盖层、dock 那行）都由折叠这条日志得到。
//!
//! 两套词表，故意分开：
//! - 条目的 `state` 只有 `start | progress | done | error`，`skipped`/`blocked`/`cached`
//!   是各自独立的布尔位；
//! - 展示状态多了 `queued`/`running`/`interrupted`/`skipped`/`blocked`/`failed`，且是
//!   **推导出来的**（见 [`display_state`]）。
//!
//! 混用这两套是画错界面最容易的方式，所以推导逻辑只有一个函数。

use super::{super::super::util::truncate_chars, meta::WorkflowPhaseMeta, task::RunStatus};
use crate::utils::{
    glyphs::{
        DEF_BLOCKED_STATUS, DEF_DONE, DEF_FAILED, DEF_MIDDOT, DEF_QUEUED, DEF_RUNNING, DEF_SKIPPED,
        DEF_STOPPED,
    },
    time::format_duration,
};

use serde_json::Value;
use std::collections::BTreeMap;
use strum_macros::{EnumString, IntoStaticStr};

/// footer/dock 里阶段标题的截断宽度。
const FOOTER_TITLE_WIDTH: usize = 16;

/// 词尾规则里的特例（不规则或读起来更好的）。
const GERUND_OVERRIDES: &[(&str, Option<&str>)] = &[
    ("commit", Some("committing")),
    ("submit", Some("submitting")),
    ("format", Some("formatting")),
    ("setup", None),
    ("cleanup", None),
];

/// 条目原始生命周期（由运行时写入）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumString, IntoStaticStr)]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
pub(crate) enum EntryState {
    /// 刚入队/开始，对应日志里的 `state = "start"`。
    Start,
    /// 运行中的中间进度更新。
    Progress,
    /// 正常结束。
    Done,
    /// 以错误结束（是否跳过/阻塞由独立的布尔位区分）。
    Error,
}

impl EntryState {
    /// 日志里写的规范名（`start`/`progress`/`done`/`error`），与 [`EntryState::parse`] 互为逆。
    pub(crate) fn as_str(self) -> &'static str {
        self.into()
    }

    /// 解析日志里的 `state`（去首尾空白、大小写不敏感）。
    fn parse(raw: &str) -> Option<Self> {
        raw.trim().parse().ok()
    }
}

/// 展示状态（渲染用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr)]
#[strum(serialize_all = "lowercase")]
pub(crate) enum DisplayState {
    /// 已被接收但还没拿到并发槽位。
    Queued,
    /// 已开跑、尚未结束。
    Running,
    /// 成功结束。
    Done,
    /// 出错结束，且不是被跳过或被拦。
    Failed,
    /// 被用户主动跳过。
    Skipped,
    /// 因依赖失败或被拦而未执行。
    Blocked,
    /// 运行已被切断：workflow 已停但条目仍在飞行中。
    Interrupted,
}

impl DisplayState {
    /// 主题/测试用的规范名（`queued`/`interrupted` 等）。
    pub(crate) fn as_str(self) -> &'static str {
        self.into()
    }

    /// 该状态下渲染的状态字形（见 `glyphs` 里的 `DEF_*`）。
    pub(crate) fn icon(self) -> &'static str {
        match self {
            DisplayState::Queued => DEF_QUEUED,
            DisplayState::Running => DEF_RUNNING,
            DisplayState::Done => DEF_DONE,
            DisplayState::Failed => DEF_FAILED,
            DisplayState::Skipped => DEF_SKIPPED,
            DisplayState::Blocked => DEF_BLOCKED_STATUS,
            DisplayState::Interrupted => DEF_STOPPED,
        }
    }

    /// 主题键 + 缺省色。
    pub(crate) fn color(self) -> (&'static str, &'static str) {
        match self {
            DisplayState::Running => ("accent", "#8abeb7"),
            DisplayState::Done => ("success", "#b5bd68"),
            DisplayState::Queued => ("muted", "#999999"),
            DisplayState::Failed => ("error", "#cc6666"),
            DisplayState::Skipped | DisplayState::Blocked => ("warning", "#f0c674"),
            DisplayState::Interrupted => ("muted", "#999999"),
        }
    }
}

/// 一个 agent 的进度条目。
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct AgentEntry {
    /// 运行内的位置序（与 `wf-agent-N`、journal 同一编号）。
    pub index: u64,
    /// 展示标签（显式 label 优先，否则 prompt 首行）。
    pub label: String,
    /// 未调用过 `phase()` 就启动的 agent：**缺省**（而不是 0）才是"整次运行收成一个 Agents 组"的信号。
    pub phase_index: Option<u64>,
    /// ambient `phase()` 或 `opts.phase` 的标题（无阶段为 None）。
    pub phase_title: Option<String>,
    /// 折叠后取最近一次写入的状态（None = 还没开始）。
    pub state: Option<EntryState>,
    /// 运行自身的句柄 `wf-agent-N`（只在运行时内部有意义）。
    pub agent_id: Option<String>,
    /// 管理器的记录 id（**派发时**就写，运行中即可用；检查器用它打开子会话）。
    pub record_id: Option<String>,
    /// 派发用的类型名。
    pub agent_type: Option<String>,
    /// 型号输入原文（可能是别名）。
    pub model: Option<String>,
    /// 解析后的 canonical `provider/id`。
    pub model_id: Option<String>,
    /// 思考档位。
    pub thinking: Option<String>,
    /// 失败原因（成功为 None）。
    pub error: Option<String>,
    /// 被用户 skip。
    pub skipped: bool,
    /// 未执行（依赖失败 / 被拦）。
    pub blocked: bool,
    /// 命中 journal 重放。
    pub cached: bool,
    /// 入队时间。
    pub queued_at: Option<u64>,
    /// 拿到并发槽开跑的时间。
    pub started_at: Option<u64>,
    /// 最近一次跃迁时间（卡片用它算"停滞"）。
    pub last_progress_at: Option<u64>,
    /// 运行时长。
    pub duration_ms: Option<u64>,
    /// 完整 prompt（检查器折行展示）。
    pub prompt: Option<String>,
    /// prompt 预览（截断）。
    pub prompt_preview: Option<String>,
    /// 结果预览（截断）。
    pub result_preview: Option<String>,
    /// 该 agent 的 token 数。
    pub tokens: Option<u64>,
    /// 工具调用次数。
    pub tool_calls: Option<u64>,
}

impl AgentEntry {
    /// 从一条 `workflow_agent` 日志解析条目；缺 `index` 或不是对象时返回 `None`
    /// （其余字段缺失按各自缺省/`false` 兜底，不报错）。
    fn from_value(v: &Value) -> Option<Self> {
        let obj = v.as_object()?;
        let index = obj.get("index").and_then(Value::as_u64)?;
        let str_of = |k: &str| obj.get(k).and_then(Value::as_str).map(str::to_string);
        let num_of = |k: &str| obj.get(k).and_then(Value::as_u64);
        let flag = |k: &str| obj.get(k).and_then(Value::as_bool).unwrap_or(false);

        Some(AgentEntry {
            index,
            label: str_of("label").unwrap_or_else(|| "agent".to_string()),
            phase_index: num_of("phaseIndex"),
            phase_title: str_of("phaseTitle"),
            state: str_of("state").as_deref().and_then(EntryState::parse),
            agent_id: str_of("agentId"),
            record_id: str_of("recordId"),
            agent_type: str_of("agentType"),
            model: str_of("model"),
            model_id: str_of("modelId"),
            thinking: str_of("thinking"),
            error: str_of("error"),
            skipped: flag("skipped"),
            blocked: flag("blocked"),
            cached: flag("cached"),
            queued_at: num_of("queuedAt"),
            started_at: num_of("startedAt"),
            last_progress_at: num_of("lastProgressAt"),
            duration_ms: num_of("durationMs"),
            prompt_preview: str_of("promptPreview"),
            prompt: str_of("prompt"),
            result_preview: str_of("resultPreview"),
            tokens: num_of("tokens"),
            tool_calls: num_of("toolCalls"),
        })
    }
}

/// 进度日志里的一条。
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Entry {
    /// 一个阶段事件（序号 + 标题）。
    Phase {
        /// 阶段序号，与 `phaseIndex`、阶段分组同一编号。
        index: u64,
        /// 阶段标题。
        title: String,
    },
    /// 一行 workflow 日志文本。
    Log(String),
    /// 一个 agent 的进度条目（boxed 以控制枚举体积）。
    Agent(Box<AgentEntry>),
}

/// 从 JSON 日志解析条目（形状不符的条目**静默跳过**——进度不是契约面，不该让界面报错）。
pub(crate) fn parse_entries(log: &[Value]) -> Vec<Entry> {
    let mut out = Vec::with_capacity(log.len());
    for v in log {
        match v.get("type").and_then(Value::as_str) {
            Some("workflow_phase") => {
                if let Some(index) = v.get("index").and_then(Value::as_u64) {
                    let title = v
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    out.push(Entry::Phase { index, title });
                }
            }
            Some("workflow_log") => {
                let message = v
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                out.push(Entry::Log(message));
            }
            Some("workflow_agent") => {
                if let Some(e) = AgentEntry::from_value(v) {
                    out.push(Entry::Agent(Box::new(e)));
                }
            }
            _ => {}
        }
    }
    out
}

/// 折叠后的日志。
#[derive(Debug, Clone, Default)]
pub(crate) struct Collapsed {
    /// 按 `index` 折叠（last write wins）后、按 index 升序的 agent 条目。
    pub agents: Vec<AgentEntry>,
    /// `workflow_log` 类的事件文本（按时间序）。
    pub logs: Vec<String>,
    /// 阶段下标 → 标题（补全只有 `phaseIndex` 的条目）。
    pub phase_titles: BTreeMap<u64, String>,
}

/// 把事件日志折叠成最新状态。
pub(crate) fn collapse(entries: &[Entry]) -> Collapsed {
    let mut agents: BTreeMap<u64, AgentEntry> = BTreeMap::new();
    let mut logs = Vec::new();
    let mut phase_titles = BTreeMap::new();

    for e in entries {
        match e {
            Entry::Agent(a) => _ = agents.insert(a.index, (**a).clone()),
            Entry::Log(m) => logs.push(m.clone()),
            Entry::Phase { index, title } => _ = phase_titles.insert(*index, title.clone()),
        }
    }

    Collapsed {
        agents: agents.into_values().collect(),
        logs,
        phase_titles,
    }
}

/// 推导一个 agent 该画成什么。
///
/// `workflow_active` 为 false 时，仍在飞行中的条目是**被切断**的（不是"还在跑"），所以画成 `interrupted`。
pub(crate) fn display_state(entry: &AgentEntry, workflow_active: bool) -> DisplayState {
    match entry.state {
        Some(EntryState::Done) => return DisplayState::Done,
        Some(EntryState::Error) => {
            if entry.skipped {
                return DisplayState::Skipped;
            }

            if entry.blocked {
                return DisplayState::Blocked;
            }
            return DisplayState::Failed;
        }
        _ => {}
    }

    if !workflow_active {
        return DisplayState::Interrupted;
    }

    // queued = 被接收但还没拿到槽位。完全没有 `queuedAt` 的条目早于信号量，按运行中算。
    if entry.queued_at.is_some() && entry.started_at.is_none() {
        DisplayState::Queued
    } else {
        DisplayState::Running
    }
}

/// 阶段分组状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr)]
#[strum(serialize_all = "lowercase")]
pub(crate) enum GroupStatus {
    /// 已声明但还没跑到的阶段。
    #[strum(serialize = "not-started")]
    NotStarted,
    /// 还有成员未结算。
    Running,
    /// 成员全部结算且全部成功。
    Done,
    /// 成员全部结算且至少一个失败。
    Failed,
}

impl GroupStatus {
    /// 规范名（`not-started`/`running`/`done`/`failed`）。
    pub(crate) fn as_str(self) -> &'static str {
        self.into()
    }

    /// 该分组状态下渲染的状态字形。
    pub(crate) fn icon(self) -> &'static str {
        match self {
            GroupStatus::NotStarted => DEF_MIDDOT,
            GroupStatus::Running => DEF_RUNNING,
            GroupStatus::Done => DEF_DONE,
            GroupStatus::Failed => DEF_FAILED,
        }
    }
}

/// 一个阶段分组（渲染器的单位）。
#[derive(Debug, Clone)]
pub(crate) struct PhaseGroup {
    /// 阶段标题。
    pub title: String,
    /// 阶段整体状态（由成员推导）。
    pub status: GroupStatus,
    /// 该阶段的 agent 条目（按 index 升序）。
    pub agents: Vec<AgentEntry>,
    /// 已完成数。
    pub done: usize,
    /// 失败数。
    pub failed: usize,
    /// 成员总数。
    pub total: usize,
    /// 成员 token 合计。
    pub tokens: u64,
    /// 阶段墙钟（最早开始 → 最晚进展）：agent 是重叠的，不能累加。
    pub duration_ms: u64,
}

/// 把一组 agent 结算成一个阶段分组：数出 done/failed/tokens，
/// 并按「最早开始 → 最晚进展」算出阶段墙钟（成员并发，不能累加时长）。
fn summarize(title: String, agents: Vec<AgentEntry>) -> PhaseGroup {
    let mut done = 0;
    let mut failed = 0;
    let mut tokens = 0u64;
    let mut min_start: Option<u64> = None;
    let mut max_progress = 0u64;

    for a in &agents {
        match a.state {
            Some(EntryState::Done) => done += 1,
            Some(EntryState::Error) => failed += 1,
            _ => {}
        }
        tokens += a.tokens.unwrap_or(0);
        if let Some(s) = a.started_at {
            min_start = Some(min_start.map_or(s, |m: u64| m.min(s)));
            let last = a.last_progress_at.unwrap_or(s);
            max_progress = max_progress.max(last);
        }
    }

    let total = agents.len();
    let finished = done + failed == total && total > 0;
    let status = if finished {
        if failed > 0 {
            GroupStatus::Failed
        } else {
            GroupStatus::Done
        }
    } else {
        GroupStatus::Running
    };

    let duration_ms = min_start.map_or(0, |m| max_progress.saturating_sub(m));
    PhaseGroup {
        title,
        status,
        agents,
        done,
        failed,
        total,
        tokens,
        duration_ms,
    }
}

/// 已声明但尚未跑到的阶段占位分组（`NotStarted`，无成员、无统计）。
fn placeholder(title: &str) -> PhaseGroup {
    PhaseGroup {
        title: title.to_string(),
        status: GroupStatus::NotStarted,
        agents: Vec::new(),
        done: 0,
        failed: 0,
        total: 0,
        tokens: 0,
        duration_ms: 0,
    }
}

/// 按阶段归并：`meta.phases` 与观察到的 `phase()` **模糊**匹配（任一方是另一方的前缀即算同一个，
/// 因为脚本可能对着声明的 `{ title: "Review changed files" }` 调 `phase("Review")`）。
/// 已声明但没跑到的阶段画成 not-started 占位；观察到的、未声明的分组追加在后面——
/// 这就是"一个没在 `meta` 里声明的 `phase()` 自己成一组"。
/// 注意：命中时分组用的是**观察到的**标题（脚本里 `phase("Review")` 那个），
/// 声明的长标题只参与匹配与排序；声明了但没跑到的才显示声明标题。
fn merge_phases(
    declared: &[WorkflowPhaseMeta],
    observed: Vec<(u64, String, Vec<AgentEntry>)>,
) -> Vec<PhaseGroup> {
    let normalize = |s: &str| s.to_lowercase().trim().to_string();
    let mut consumed = vec![false; observed.len()];
    let mut merged = Vec::new();

    for phase in declared {
        let wanted = normalize(&phase.title);
        let mut hit = None;
        for (i, (_, title, _)) in observed.iter().enumerate() {
            if consumed[i] {
                continue;
            }

            let actual = normalize(title);
            if actual == wanted || actual.starts_with(&wanted) || wanted.starts_with(&actual) {
                hit = Some(i);
                break;
            }
        }

        match hit {
            Some(i) => {
                consumed[i] = true;
                let (_, title, agents) = observed[i].clone();
                merged.push(summarize(title, agents));
            }
            None => merged.push(placeholder(&phase.title)),
        }
    }

    for (i, (_, title, agents)) in observed.into_iter().enumerate() {
        if !consumed[i] {
            merged.push(summarize(title, agents));
        }
    }
    merged
}

/// 构造渲染器要走的阶段分组。
///
/// 没有任何声明或观察到的阶段时，全部 agent 收成一个 "Agents" 组，让树至少有一层结构。
pub(crate) fn build_phase_groups(
    entries: &[Entry],
    declared: Option<&[WorkflowPhaseMeta]>,
) -> Vec<PhaseGroup> {
    let collapsed = collapse(entries);
    let agents = collapsed.agents;
    let phase_titles = collapsed.phase_titles;
    let observed = group_by_phase(&agents, &phase_titles);

    let mut merged = merge_phases(declared.unwrap_or(&[]), observed);
    if merged.is_empty() && !agents.is_empty() {
        return vec![summarize("Agents".to_string(), agents)];
    }

    // 声明了阶段但产生了无阶段 agent：否则那些 agent 会被占位符挤出树外。上游同样有这道兜底。
    if !agents.is_empty() && !merged.iter().any(|g| g.total > 0) {
        let extra = summarize("Agents".to_string(), agents.clone());
        merged.push(extra);
    }

    merged
}

/// 按 `phase_index` 把 agent 归并成 (序号, 标题, 成员) 列表，按序号升序；
/// 标题优先取成员自带的 `phase_title`，否则查 `phase_titles`，都没有则回落成 `Phase N`。
/// 没有任何 agent 带阶段信息时返回空（交给调用方收成单个 Agents 组）。
fn group_by_phase(
    agents: &[AgentEntry],
    phase_titles: &BTreeMap<u64, String>,
) -> Vec<(u64, String, Vec<AgentEntry>)> {
    if !agents.iter().any(|a| a.phase_index.is_some()) {
        return Vec::new();
    }

    let mut by_phase: BTreeMap<u64, Vec<AgentEntry>> = BTreeMap::new();
    for a in agents {
        by_phase
            .entry(a.phase_index.unwrap_or(0))
            .or_default()
            .push(a.clone());
    }

    by_phase
        .into_iter()
        .map(|(index, agents)| {
            let title = agents
                .first()
                .and_then(|a| a.phase_title.clone())
                .or_else(|| phase_titles.get(&index).cloned())
                .unwrap_or_else(|| format!("Phase {index}"));
            (index, title, agents)
        })
        .collect()
}

/// 头部计数。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Stats {
    /// 已完成数。
    pub done: usize,
    /// 失败数。
    pub failed: usize,
    /// 是否仍有运行中的 agent。
    pub running: bool,
    /// 总数。
    pub total: usize,
    /// 已开始数。
    pub started: usize,
    /// 是否全部结算。
    pub complete: bool,
}

/// 从日志（+ 运行时**已排定**的 agent 数）汇总头部计数。
///
/// `total` 取 `max(agent_count, 折叠后的 agent 数)`：
/// fan-out 会在 agent 开跑前先报出规模，这样总数不会随着它们陆续出现而往上爬。
///
/// **先折叠再数**：日志是 append-only 的（一个 agent 有 start/progress/done 多条），
/// 按条目数计，会把 3 个 agent 数成 9 个。折叠按 `index` last-write-wins，重发的行只算一次。
pub(crate) fn stats(log: &[Value], agent_count: u64) -> Stats {
    let collapsed = collapse(&parse_entries(log));
    let agents = collapsed.agents;
    let mut seen = 0usize;
    let mut done = 0usize;
    let mut failed = 0usize;
    let mut started = 0usize;
    let mut any_live = false;

    for a in &agents {
        seen += 1;
        match a.state {
            Some(EntryState::Done) => {
                done += 1;
                started += 1;
            }
            Some(EntryState::Error) => {
                failed += 1;
                started += 1;
            }
            _ => {
                any_live = true;
                // 除非"确实还在等槽位"，否则算已开始
                if a.started_at.is_some() || a.queued_at.is_none() {
                    started += 1;
                }
            }
        }
    }

    let total = (agent_count as usize).max(seen);
    Stats {
        done,
        failed,
        running: any_live,
        total,
        started,
        complete: !any_live && seen > 0 && done + failed >= total,
    }
}

/// 把阶段标题读成"正在做什么"：`Scan` → `Scanning`。
///
/// 只用在 footer/dock 那种"现在在干什么"的行上；不是普通短词就原样返回。
pub(crate) fn gerund(word: &str) -> String {
    let lower = word.to_lowercase();
    let is_candidate =
        word.len() >= 3 && word.len() <= 12 && word.chars().all(|c| c.is_ascii_alphabetic());
    if !is_candidate {
        return word.to_string();
    }
    if let Some((_, over)) = GERUND_OVERRIDES.iter().find(|(k, _)| *k == lower) {
        return match over {
            None => word.to_string(),
            Some(g) => {
                let mut s = word[..1].to_string();
                s.push_str(&g[1..]);
                s
            }
        };
    }
    if lower.ends_with("ing") {
        return word.to_string();
    }
    if lower.ends_with("ie") {
        return format!("{}ying", &word[..word.len() - 2]);
    }
    if lower.ends_with('e') && !lower.ends_with("ee") && !lower.ends_with("ye") {
        return format!("{}ing", &word[..word.len() - 1]);
    }

    // 短 CVC 词双写尾辅音：run → running（`w`/`x`/`y` 从不双写）。
    let bytes = lower.as_bytes();
    let vowels = b"aeiou";
    if bytes.len() <= 4 && bytes.len() >= 3 {
        let (c1, c2, last) = (
            bytes[bytes.len() - 3],
            bytes[bytes.len() - 2],
            bytes[bytes.len() - 1],
        );
        if !vowels.contains(&c1)
            && vowels.contains(&c2)
            && !vowels.contains(&last)
            && !b"wxy".contains(&last)
        {
            return format!("{word}{}ing", last as char);
        }
    }
    format!("{word}ing")
}

/// "这个 run 现在在干什么"的标签。
///
/// 一个活跃阶段显示位置；两个并发的阶段用 `&` 连接（无屏障的 pipeline 常有这种时候）。
pub(crate) fn footer_phase_label(
    titles: &[String],
    position_start: usize,
    total_phases: usize,
) -> String {
    let titles: Vec<String> = titles.iter().map(|t| gerund(t)).collect();
    match titles.len() {
        0 => String::new(),
        1 => format!(
            "{} ({position_start}/{total_phases})",
            truncate_chars(&titles[0], FOOTER_TITLE_WIDTH)
        ),
        _ => titles
            .iter()
            .map(|t| truncate_chars(t, FOOTER_TITLE_WIDTH))
            .collect::<Vec<_>>()
            .join(" & "),
    }
}

/// 覆盖层/卡片头一行要的信息。
pub(crate) struct RunHeader {
    /// 运行名（workflow `meta.name`）。
    pub name: String,
    /// 副标题（id / 来源等）。
    pub subtext: String,
    /// `3/7 agents · 1m12s · done` 形态的统计串。
    pub stats: String,
}

/// `3/7 agents · 1m12s · done`。
#[allow(clippy::too_many_arguments)]
pub(crate) fn header(
    name: &str,
    description: &str,
    status: RunStatus,
    groups: &[PhaseGroup],
    agent_count: u64,
    elapsed_ms: u64,
) -> RunHeader {
    let suffix = match status {
        RunStatus::Completed => " · done",
        RunStatus::Killed => " · stopped",
        RunStatus::Failed => " · failed",
        RunStatus::Paused => " · paused",
        RunStatus::Running => "",
    };
    let mut done_agents = 0usize;
    let mut total_agents = 0usize;
    for g in groups {
        done_agents += g.done;
        total_agents += g.total;
    }
    total_agents = total_agents.max(agent_count as usize).max(done_agents);
    let plural = if total_agents == 1 { "agent" } else { "agents" };

    RunHeader {
        name: name.to_string(),
        subtext: description.to_string(),
        stats: format!(
            "{done_agents}/{total_agents} {plural} · {}{suffix}",
            format_duration(elapsed_ms)
        ),
    }
}

/// 当前"正在跑"的阶段标题（dock 那行用）。
pub(crate) fn active_phase_titles(groups: &[PhaseGroup]) -> Vec<String> {
    groups
        .iter()
        .filter(|g| g.status == GroupStatus::Running && g.total > 0)
        .map(|g| g.title.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn agent(index: u64, state: &str, extra: Value) -> Value {
        let mut obj = json!({
            "type": "workflow_agent",
            "index": index,
            "label": format!("agent-{index}"),
            "state": state,
        });
        if let (Some(base), Some(add)) = (obj.as_object_mut(), extra.as_object()) {
            for (k, v) in add {
                base.insert(k.clone(), v.clone());
            }
        }
        obj
    }

    #[test]
    fn collapse_keeps_last_write_per_index() {
        let log = vec![
            json!({ "type": "workflow_phase", "index": 0, "title": "Scan" }),
            json!({ "type": "workflow_log", "message": "hi" }),
            agent(0, "start", json!({ "queuedAt": 10 })),
            agent(0, "progress", json!({ "queuedAt": 10, "startedAt": 20 })),
            agent(
                0,
                "done",
                json!({ "queuedAt": 10, "startedAt": 20, "tokens": 5 }),
            ),
            agent(1, "progress", json!({ "queuedAt": 30, "startedAt": 40 })),
        ];
        let entries = parse_entries(&log);
        let c = collapse(&entries);
        assert_eq!(c.agents.len(), 2, "同 index 只留最后一条");
        assert_eq!(c.agents[0].state, Some(EntryState::Done));
        assert_eq!(c.agents[0].tokens, Some(5));
        assert_eq!(c.agents[1].state, Some(EntryState::Progress));
        assert_eq!(c.logs, vec!["hi".to_string()]);
        assert_eq!(c.phase_titles.get(&0).map(String::as_str), Some("Scan"));
    }

    #[test]
    fn display_state_covers_queued_running_and_interrupted() {
        let queued = AgentEntry {
            queued_at: Some(1),
            ..Default::default()
        };
        assert_eq!(display_state(&queued, true), DisplayState::Queued);
        let running = AgentEntry {
            queued_at: Some(1),
            started_at: Some(2),
            ..Default::default()
        };
        assert_eq!(display_state(&running, true), DisplayState::Running);
        // run 停了以后仍在飞行中的条目 = 被切断，不是"还在跑"
        assert_eq!(display_state(&running, false), DisplayState::Interrupted);
        let done = AgentEntry {
            state: Some(EntryState::Done),
            ..Default::default()
        };
        assert_eq!(
            display_state(&done, false),
            DisplayState::Done,
            "已终结不受 active 影响"
        );
        let skipped = AgentEntry {
            state: Some(EntryState::Error),
            skipped: true,
            ..Default::default()
        };
        assert_eq!(display_state(&skipped, true), DisplayState::Skipped);
    }

    #[test]
    fn phase_groups_merge_declared_with_observed_fuzzily() {
        let phases = vec![
            WorkflowPhaseMeta {
                title: "Review changed files".into(),
                detail: None,
                model: None,
            },
            WorkflowPhaseMeta {
                title: "Never reached".into(),
                detail: None,
                model: None,
            },
        ];
        let log = vec![
            json!({ "type": "workflow_phase", "index": 0, "title": "Review" }),
            json!({ "type": "workflow_phase", "index": 1, "title": "Undeclared" }),
            agent(
                0,
                "done",
                json!({ "phaseIndex": 0, "phaseTitle": "Review", "startedAt": 5, "lastProgressAt": 15 }),
            ),
            agent(
                1,
                "error",
                json!({ "phaseIndex": 1, "phaseTitle": "Undeclared", "error": "boom" }),
            ),
        ];
        let entries = parse_entries(&log);
        let groups = build_phase_groups(&entries, Some(&phases));
        assert_eq!(groups.len(), 3);
        // 模糊匹配：phase("Review") 归到声明的 "Review changed files"，
        // 但分组标题用观察到的那个（上游 `summarize(match)` 如此）
        assert_eq!(groups[0].title, "Review");
        assert_eq!(groups[0].status, GroupStatus::Done);
        assert_eq!(groups[0].done, 1);
        assert_eq!(groups[0].duration_ms, 10);
        // 声明了但没跑到
        assert_eq!(groups[1].title, "Never reached");
        assert_eq!(groups[1].status, GroupStatus::NotStarted);
        assert_eq!(groups[1].total, 0);
        // 未声明的自己成组
        assert_eq!(groups[2].title, "Undeclared");
        assert_eq!(groups[2].status, GroupStatus::Failed);
        assert_eq!(groups[2].failed, 1);
    }

    #[test]
    fn phase_groups_fall_back_to_a_single_agents_group() {
        let log = vec![agent(0, "done", json!({})), agent(1, "done", json!({}))];
        let entries = parse_entries(&log);
        let groups = build_phase_groups(&entries, None);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].title, "Agents");
        assert_eq!(groups[0].total, 2);
        assert_eq!(groups[0].status, GroupStatus::Done);
    }

    #[test]
    fn declared_phases_with_unphased_agents_keep_both() {
        let phases = vec![WorkflowPhaseMeta {
            title: "Scan".into(),
            detail: None,
            model: None,
        }];
        // 声明了阶段，但 agent 没归到任何 phase()
        let log = vec![agent(0, "done", json!({}))];
        let entries = parse_entries(&log);
        let groups = build_phase_groups(&entries, Some(&phases));
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].status, GroupStatus::NotStarted);
        assert_eq!(groups[1].title, "Agents");
        assert_eq!(groups[1].total, 1);
    }

    /// 日志是 append-only 的：同一个 agent 会有 start/progress/done 多条，
    /// 计数必须**先折叠**，否则 3 个 agent 会被数成 9 个。
    #[test]
    fn stats_counts_each_agent_once_despite_multiple_entries() {
        let log = vec![
            agent(0, "start", json!({ "queuedAt": 1 })),
            agent(0, "progress", json!({ "queuedAt": 1, "startedAt": 2 })),
            agent(0, "done", json!({ "queuedAt": 1, "startedAt": 2 })),
            agent(1, "start", json!({ "queuedAt": 1 })),
            agent(1, "progress", json!({ "queuedAt": 1, "startedAt": 2 })),
            agent(
                1,
                "error",
                json!({ "queuedAt": 1, "startedAt": 2, "error": "x" }),
            ),
        ];
        let s = stats(&log, 0);
        assert_eq!(s.total, 2, "两次重发不该各算一个 agent");
        assert_eq!(s.done, 1);
        assert_eq!(s.failed, 1);
        assert!(s.complete);
    }

    #[test]
    fn stats_counts_started_queued_and_total() {
        let log = vec![
            agent(0, "done", json!({})),
            agent(1, "error", json!({})),
            agent(2, "progress", json!({ "queuedAt": 1, "startedAt": 2 })),
            agent(3, "start", json!({ "queuedAt": 3 })),
        ];
        let s = stats(&log, 10);
        assert_eq!(s.done, 1);
        assert_eq!(s.failed, 1);
        assert_eq!(s.started, 3, "排队的那个不算已开始");
        assert_eq!(s.total, 10, "运行时已排定的规模优先，避免总数往上爬");
        assert!(s.running);
        assert!(!s.complete);
    }

    #[test]
    fn gerund_handles_regular_and_irregular_words() {
        assert_eq!(gerund("Scan"), "Scanning");
        assert_eq!(gerund("Audit"), "Auditing");
        assert_eq!(gerund("Verify"), "Verifying");
        assert_eq!(gerund("commit"), "committing");
        assert_eq!(gerund("Fix"), "Fixing");
        assert_eq!(gerund("Review"), "Reviewing");
        // 非短词/非纯字母原样返回
        assert_eq!(gerund("Review changed files"), "Review changed files");
        assert_eq!(gerund("Scanning"), "Scanning");
    }

    #[test]
    fn footer_label_joins_concurrent_phases() {
        assert_eq!(footer_phase_label(&[], 0, 0), "");
        assert_eq!(
            footer_phase_label(&["Scan".to_string()], 1, 3),
            "Scanning (1/3)"
        );
        assert_eq!(
            footer_phase_label(&["Scan".to_string(), "Audit".to_string()], 1, 3),
            "Scanning & Auditing"
        );
    }

    /// strum 派生的规范名与解析：写日志与读日志用的是同一份映射。
    #[test]
    fn strum_enum_conversions() {
        // EntryState：双向（写日志 <-> 解析日志）
        assert_eq!(EntryState::Start.as_str(), "start");
        assert_eq!(EntryState::Progress.as_str(), "progress");
        assert_eq!(EntryState::Done.as_str(), "done");
        assert_eq!(EntryState::Error.as_str(), "error");
        assert_eq!(EntryState::parse("done"), Some(EntryState::Done));
        assert_eq!(EntryState::parse(" PROGRESS "), Some(EntryState::Progress));
        assert_eq!(EntryState::parse("nope"), None);
        // 日志里的每个规范名都必须能解析回来（否则界面静默丢行）
        for s in ["start", "progress", "done", "error"] {
            assert_eq!(EntryState::parse(s).map(EntryState::as_str), Some(s));
        }

        // DisplayState / GroupStatus：单向（渲染用）
        assert_eq!(DisplayState::Queued.as_str(), "queued");
        assert_eq!(DisplayState::Interrupted.as_str(), "interrupted");
        assert_eq!(GroupStatus::NotStarted.as_str(), "not-started");
        assert_eq!(GroupStatus::Running.as_str(), "running");
        assert_eq!(GroupStatus::Done.as_str(), "done");
        assert_eq!(GroupStatus::Failed.as_str(), "failed");
    }
}
