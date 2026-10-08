//! 运行检查器
//!
//! 两栏：左边是这次运行的树（阶段分组 / 扁平两种视图，`f` 切换），右边是选中那一行的详情。
//! 用 **Fullscreen 覆盖层**，而"两栏"由本模块按终端宽度自己拼。
//! 核心的 [`OverlayView`] 只是一列带样式的行，不认识"栏"，
//! 所以宽度从 `Extension::overlay_view(id, cols)` 传进来。
//!
//! 控制面（`p` 暂停 / `s` 跳过 / `r` 重跑 / `x` 停止）来自 kernel 交回的
//! [`super::bridge::WorkflowControl`]；`c` 打开选中 agent 的**会话查看器**），
//! 所以这一屏同时是"看为什么失败"和"跳进它自己的对话"的入口。

use super::{
    super::notify,
    progress::{self, AgentEntry, GroupStatus, PhaseGroup},
    task::WorkflowRun,
};
use crate::{
    core::extensions::{DockLine, DockSpan, OverlaySize, OverlayView},
    extensions::util::truncate_chars,
    utils::{
        display::{display_width, fit_display, wrap_capped},
        glyphs::{DEF_ARROW_UP_DOWN, DEF_SELECTED_MARK},
        time::format_duration,
    },
};

/// prompt 折行展示的行数上限；超过时在末行末尾标 `...`。
pub(crate) const MAX_PROMPT_LINES: usize = 10;
/// prompt 标签（单行放不下时独占一行）。
const PROMPT_LABEL: &str = "prompt:";
/// prompt 单行内联时的标签前缀。
const PROMPT_PREFIX: &str = "prompt: ";
/// prompt 多行正文的缩进（加上分隔线后的那格，内容距 `│` 3 格）。
const PROMPT_CONTENT_INDENT: &str = "  ";

/// 检查器的 UI 状态（由 `fleet` 的覆盖层状态持有）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct InspectorState {
    /// 选中行（可见行序）
    pub selected: usize,
    /// 扁平视图（只列 agent，不分组）
    pub flat: bool,
}

/// 左栏的一行（键盘导航与详情栏都按它找目标）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Row {
    /// 阶段分组标题
    Phase { group: usize },
    /// 一个 agent
    Agent { index: u64, group: usize },
}

/// 一次运行在当前视图下的可见行（顺序即渲染顺序）。
pub(crate) fn rows(run: &WorkflowRun, state: InspectorState) -> Vec<Row> {
    let mut rows = Vec::new();
    let groups = run.groups();

    for (gi, group) in groups.iter().enumerate() {
        if !state.flat {
            rows.push(Row::Phase { group: gi });
        }

        for agent in &group.agents {
            rows.push(Row::Agent {
                index: agent.index,
                group: gi,
            });
        }
    }
    rows
}

/// 选中的那一行（越界时夹到合法范围）。
pub(crate) fn selected_row(run: &WorkflowRun, state: InspectorState) -> Option<Row> {
    let rows = rows(run, state);
    if rows.is_empty() {
        return None;
    }
    Some(rows[state.selected.min(rows.len() - 1)].clone())
}

/// 选中行的 agent 序号（供 skip/retry/打开会话用）。
pub(crate) fn selected_agent(run: &WorkflowRun, state: InspectorState) -> Option<u64> {
    match selected_row(run, state)? {
        Row::Agent { index, .. } => Some(index),
        Row::Phase { .. } => None,
    }
}

/// 选中 agent 的管理器记录 id（`c` 用它打开会话查看器）。
pub(crate) fn agent_record_id(run: &WorkflowRun, index: u64) -> Option<String> {
    run.groups()
        .iter()
        .flat_map(|g| g.agents.iter())
        .find(|a| a.index == index)
        .and_then(|a| a.record_id.clone())
}

/// 渲染检查器。
pub(crate) fn view(run: &WorkflowRun, state: InspectorState, cols: u16) -> OverlayView {
    let cols = cols.max(40) as usize;
    let left_w = (cols * 2 / 5).clamp(24, 60);
    let right_w = cols.saturating_sub(left_w + 3).max(16);
    let rows = rows(run, state);
    let selected = if rows.is_empty() {
        0
    } else {
        state.selected.min(rows.len() - 1)
    };
    let groups = run.groups();
    let header = run.header();

    // 左栏
    let mut left: Vec<(String, &'static str, &'static str)> = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        let (text, key, fallback) = match row {
            Row::Phase { group } => {
                let g = &groups[*group];
                let (key, fallback) = group_color(g.status);
                (
                    format!("{} {} {}/{}", g.status.icon(), g.title, g.done, g.total),
                    key,
                    fallback,
                )
            }
            Row::Agent { index, group } => {
                let Some(a) = groups[*group].agents.iter().find(|a| a.index == *index) else {
                    continue;
                };

                let ds = progress::display_state(a, !run.status.is_terminal());
                let (key, fallback) = ds.color();
                let mut text = format!("  {} {}", ds.icon(), truncate_chars(&a.label, 48));

                if let Some(ms) = agent_duration_ms(a) {
                    text.push_str(&format!(" · {}", format_duration(ms)));
                }

                (text, key, fallback)
            }
        };

        let marker = if i == selected {
            DEF_SELECTED_MARK
        } else {
            "  "
        };
        let line = format!("{marker}{text}");
        left.push((line, key, fallback));
    }

    if left.is_empty() {
        left.push(("(no agents yet)".to_string(), "dim", "#666666"));
    }

    // 右栏：选中行的详情；没有选中行就是这次运行的摘要
    let right = match selected_row(run, state) {
        Some(Row::Agent { index, group }) => groups[group]
            .agents
            .iter()
            .find(|a| a.index == index)
            .map(|a| agent_detail(a, run, right_w))
            .unwrap_or_default(),
        Some(Row::Phase { group }) => phase_detail(&groups[group]),
        None => run_detail(run),
    };

    // 两栏拼行
    let height = left.len().max(right.len());
    let mut lines: Vec<DockLine> = Vec::with_capacity(height + 2);

    // 标题行：`▶ name id · status · done/total · elapsed`
    let (hkey, hfallback) = run.status.color();
    lines.push(vec![
        DockSpan::new(hkey, hfallback, format!("{} ", run.status.icon())),
        DockSpan::new("accent", "#8abeb7", run.name.clone()),
        DockSpan::new("dim", "#666666", format!(" {} · {}", run.id, header.stats)),
    ]);

    if let Some(from) = run.resumed_from.as_deref() {
        let n = super::notify::replayed_count(run);
        lines.push(vec![DockSpan::new(
            "dim",
            "#666666",
            format!("  resumed from {from} · {n} replayed"),
        )]);
    }

    for i in 0..height {
        let mut spans: Vec<DockSpan> = Vec::new();
        match left.get(i) {
            Some((text, key, fallback)) => {
                // 用**显示宽度**算填充：CJK/宽字符下 `chars().count()` 会低估，
                // 导致这一行的 `│` 比别人靠右（曾经首行就是这么歪的）。
                let text = fit_display(text, left_w);
                let pad = left_w.saturating_sub(display_width(&text));
                spans.push(DockSpan::new(key, *fallback, text));
                spans.push(DockSpan::plain(" ".repeat(pad + 1)));
            }
            None => spans.push(DockSpan::plain(" ".repeat(left_w + 1))),
        }
        spans.push(DockSpan::new("dim", "#666666", "│ "));

        if let Some((text, key, fallback)) = right.get(i) {
            spans.push(DockSpan::new(key, *fallback, fit_display(text, right_w)));
        }

        lines.push(spans);
    }

    OverlayView {
        title: format!("Workflow {} · {}", run.name, run.status.as_str()),
        lines,
        footer: footer(run, state),
        input: None,
        editor: None,
        size: OverlaySize::Fullscreen,
        header: Vec::new(),
        messages: Vec::new(),
        selected: None,
        input_focus: false,
    }
}

/// 底部键位提示：只列**此刻真能用**的键（由运行状态与选中行共同决定）。
///
/// 键的可达性：
/// - `↑↓`/`f`/`Esc` 始终可用；
/// - `p` 需要 control（kernel 已交回控制面）且运行未终结；
/// - `s` 只对还没结算的 agent 有效（跳过已结算的是 no-op）；
/// - `r` 只对**正在跑**的 agent 有效（结算后它的值已交给脚本）；
/// - `c` 只在选中 agent 已有 `recordId` 时有效；
/// - `x` 只要运行未终结就有效。
fn footer(run: &WorkflowRun, state: InspectorState) -> Vec<(String, String)> {
    let live = !run.status.is_terminal();
    let ctl = run.control.as_ref();

    let mut hints: Vec<(String, String)> = vec![
        (DEF_ARROW_UP_DOWN.to_string(), "select".to_string()),
        ("f".to_string(), "flat/phases".to_string()),
    ];

    if live && let Some(ctl) = ctl {
        hints.push((
            "p".to_string(),
            if ctl.is_paused() { "resume" } else { "pause" }.to_string(),
        ));
    }

    // 选中行决定 skip / retry / open 是否可用（阶段行上没有这些动作）。
    let selected = selected_agent_entry(run, state);
    if let Some(entry) = selected.as_ref() {
        let ds = progress::display_state(entry, live);

        if live && ctl.is_some() {
            if matches!(
                ds,
                progress::DisplayState::Queued | progress::DisplayState::Running
            ) {
                hints.push(("s".to_string(), "skip".to_string()));
            }

            if ds == progress::DisplayState::Running {
                hints.push(("r".to_string(), "retry".to_string()));
            }
        }

        if entry.record_id.is_some() {
            hints.push(("c".to_string(), "open agent".to_string()));
        }
    }

    if live {
        hints.push(("x".to_string(), "stop".to_string()));
    }

    hints.push(("Esc".to_string(), "back".to_string()));
    hints
}

/// 选中行的 agent 条目（阶段行/空行时 None）。
///
/// `groups()` 每次返回新建的集合，所以这边只能克隆出条目（字段少，代价可忽略）。
fn selected_agent_entry(run: &WorkflowRun, state: InspectorState) -> Option<AgentEntry> {
    let groups = run.groups();
    match selected_row(run, state)? {
        Row::Agent { index, group } => groups
            .get(group)
            .and_then(|g| g.agents.iter().find(|a| a.index == index))
            .cloned(),
        Row::Phase { .. } => None,
    }
}

/// 阶段状态对应的配色：(主题色 key, 无主题时的 hex 回退)。
fn group_color(status: GroupStatus) -> (&'static str, &'static str) {
    match status {
        GroupStatus::NotStarted => ("muted", "#999999"),
        GroupStatus::Running => ("accent", "#8abeb7"),
        GroupStatus::Done => ("success", "#b5bd68"),
        GroupStatus::Failed => ("error", "#cc6666"),
    }
}

/// agent 耗时毫秒：优先用已记录的 duration，否则取 start→last_progress 差值；
/// 两者都缺时返回 `None`。
fn agent_duration_ms(a: &progress::AgentEntry) -> Option<u64> {
    a.duration_ms.or(match (a.started_at, a.last_progress_at) {
        (Some(s), Some(l)) => Some(l.saturating_sub(s)),
        _ => None,
    })
}

/// 右栏：一个 agent 的详情。
///
/// `right_w` 是右栏可用宽度（列）：prompt 按它折行，且总行数封顶（见 [`MAX_PROMPT_LINES`]）。
fn agent_detail(
    a: &AgentEntry,
    run: &WorkflowRun,
    right_w: usize,
) -> Vec<(String, &'static str, &'static str)> {
    let ds = progress::display_state(a, !run.status.is_terminal());
    let (ds_key, ds_fallback) = ds.color();

    let mut out: Vec<(String, &'static str, &'static str)> = Vec::new();
    out.push((format!("{} {}", ds.icon(), a.label), ds_key, ds_fallback));
    out.push((format!("state: {}", ds.as_str()), "dim", "#666666"));

    if let Some(t) = a.agent_type.as_deref() {
        out.push((format!("type: {t}"), "dim", "#666666"));
    }
    if let Some(id) = a.record_id.as_deref() {
        out.push((format!("record: {id}"), "dim", "#666666"));
    }
    if let Some(m) = a.model.as_deref() {
        out.push((format!("model: {m}"), "dim", "#666666"));
    }

    let mut usage: Vec<String> = Vec::new();
    if let Some(ms) = agent_duration_ms(a) {
        usage.push(format_duration(ms));
    }
    if a.tokens.unwrap_or(0) > 0 {
        usage.push(format!(
            "{} tok",
            notify::format_tokens(a.tokens.unwrap_or(0))
        ));
    }
    if let Some(tc) = a.tool_calls.filter(|t| *t > 0) {
        usage.push(format!("{tc} tools"));
    }
    if !usage.is_empty() {
        out.push((usage.join(" · "), "dim", "#666666"));
    }
    if let Some(p) = a.phase_title.as_deref() {
        out.push((format!("phase: {p}"), "dim", "#666666"));
    }
    if a.cached {
        out.push(("(from resume journal)".to_string(), "muted", "#999999"));
    }
    if let Some(err) = a.error.as_deref() {
        out.push((String::new(), "dim", "#666666"));
        out.push((truncate_chars(err, 200), "error", "#cc6666"));
    }

    // 折行展示（单行时很容易被宽度截断），但最多 10 行，超出时在末尾标 `...`。
    // 多行时 `prompt:` 独占一行，正文缩进（右栏渲染会 trim，所以靠一个不 trim 的 [`fit_display`] 保留前导空白）。
    if let Some(p) = a.prompt.as_deref().or(a.prompt_preview.as_deref()) {
        out.push((String::new(), "dim", "#666666"));
        let content_w = right_w.saturating_sub(PROMPT_CONTENT_INDENT.len()).max(1);
        let lines = wrap_capped(p, content_w, MAX_PROMPT_LINES);

        if lines.len() == 1 && display_width(&lines[0]) + PROMPT_PREFIX.len() <= right_w {
            out.push((format!("{PROMPT_PREFIX}{}", lines[0]), "dim", "#666666"));
        } else {
            out.push((PROMPT_LABEL.to_string(), "dim", "#666666"));

            for line in &lines {
                out.push((format!("{PROMPT_CONTENT_INDENT}{line}"), "dim", "#666666"));
            }
        }
    }

    if let Some(r) = a.result_preview.as_deref() {
        out.push((String::new(), "dim", "#666666"));
        out.push((
            format!("result: {}", truncate_chars(r, 300)),
            "dim",
            "#666666",
        ));
    }
    out
}

/// 右栏：单个 phase 的详情行，元素为 (文本, 主题色 key, hex 回退)。
fn phase_detail(group: &PhaseGroup) -> Vec<(String, &'static str, &'static str)> {
    let (key, fallback) = group_color(group.status);
    let mut out = vec![
        (
            format!("{} {}", group.status.icon(), group.title),
            key,
            fallback,
        ),
        (
            format!("{}/{} agents", group.done, group.total),
            "dim",
            "#666666",
        ),
    ];

    if group.failed > 0 {
        out.push((format!("{} failed", group.failed), "error", "#cc6666"));
    }
    if group.tokens > 0 {
        out.push((
            format!("{} tok", notify::format_tokens(group.tokens)),
            "dim",
            "#666666",
        ));
    }
    if group.duration_ms > 0 {
        out.push((format_duration(group.duration_ms), "dim", "#666666"));
    }
    out
}

/// 右栏：整个工作流运行的详情行（标题/统计/脚本路径/错误/最近日志），
/// 元素为 (文本, 主题色 key, hex 回退)。
fn run_detail(run: &WorkflowRun) -> Vec<(String, &'static str, &'static str)> {
    let header = run.header();
    let mut out: Vec<(String, &'static str, &'static str)> = vec![
        (header.name.clone(), "accent", "#8abeb7"),
        (header.stats.clone(), "dim", "#666666"),
    ];
    if !header.subtext.is_empty() {
        out.push((truncate_chars(&header.subtext, 200), "dim", "#666666"));
    }
    if let Some(p) = run.script_path.as_deref() {
        out.push((format!("script: {p}"), "dim", "#666666"));
    }
    if let Some(err) = run.error.as_deref() {
        out.push((truncate_chars(err, 200), "error", "#cc6666"));
    }

    let entries = progress::parse_entries(&run.log());
    for log in progress::collapse(&entries).logs.iter().rev().take(6).rev() {
        out.push((format!("· {}", truncate_chars(log, 160)), "dim", "#666666"));
    }

    out
}

/// 运行状态名（`EntryState` 与 `RunStatus` 之间的桥，供测试用）。
#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::subagent::workflow::task;

    fn text(line: &DockLine) -> String {
        line.iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn view_lays_out_two_columns_with_the_selected_agent_detail() {
        let _g = super::super::super::test_lock();
        task::reset_all();
        let id = task::new_id();
        let _abort = task::register(&id, "demo", "why", None, None, None);
        task::set_agent_count(&id, 2);
        let run = task::find(&id).unwrap();
        {
            let mut log = run.progress.lock().unwrap();
            log.push(serde_json::json!({ "type": "workflow_phase", "index": 0, "title": "Scan" }));
            log.push(serde_json::json!({
                "type": "workflow_agent", "index": 0, "label": "scan a", "state": "done",
                "phaseIndex": 0, "phaseTitle": "Scan", "recordId": "ab12cd34",
                "startedAt": 1, "lastProgressAt": 11, "tokens": 500, "toolCalls": 3
            }));
            log.push(serde_json::json!({
                "type": "workflow_agent", "index": 1, "label": "scan b", "state": "error",
                "phaseIndex": 0, "phaseTitle": "Scan", "error": "provider exploded"
            }));
        }
        let run = task::find(&id).unwrap();
        let state = InspectorState {
            selected: 0,
            flat: false,
        };
        let v = view(&run, state, 100);
        assert_eq!(v.size, OverlaySize::Fullscreen);
        let lines: Vec<String> = v.lines.iter().map(text).collect();
        // 标题行带名字与 id
        assert!(
            lines[0].contains("demo") && lines[0].contains(&id),
            "{lines:?}"
        );
        // 两栏：左树右详情，中间有分隔
        assert!(lines.iter().any(|l| l.contains('│')), "{lines:?}");
        // 阶段模式有阶段行，且选中它时右栏是阶段详情
        // 组内有失败 → 组状态是 failed（done + failed == total 且有 failed）
        assert!(lines.iter().any(|l| l.contains("✗ Scan 1/2")), "{lines:?}");
        assert!(
            lines.iter().any(|l| l.contains("│ 1/2 agents")),
            "{lines:?}"
        );

        // 扁平模式：只列 agent；选中带记录的 agent → 右栏有 record/用量
        let flat = InspectorState {
            selected: 0,
            flat: true,
        };
        let v = view(&run, flat, 100);
        let lines: Vec<String> = v.lines.iter().map(text).collect();
        assert!(
            !lines.iter().any(|l| l.contains("Scan 1/2")),
            "扁平视图没有阶段行: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("record: ab12cd34")),
            "{lines:?}"
        );
        assert!(lines.iter().any(|l| l.contains("500 tok")), "{lines:?}");

        // 选中失败的那个 agent → 右栏是它的错误
        let failed = InspectorState {
            selected: 1,
            flat: true,
        };
        let v = view(&run, failed, 100);
        let lines: Vec<String> = v.lines.iter().map(text).collect();
        assert!(
            lines.iter().any(|l| l.contains("provider exploded")),
            "失败原因要出现在详情栏: {lines:?}"
        );

        // 选中行越界时夹到最后一行的详情（不 panic）
        let state = InspectorState {
            selected: 999,
            flat: true,
        };
        let _ = view(&run, state, 40);
        task::reset_all();
    }

    /// 底部键位提示只 advertised 此刻真能用的键：
    /// 运行中的 agent 没有 `recordId` → 不显示 `c`；阶段行没有 skip/retry/open。
    #[test]
    fn footer_only_advertises_usable_keys() {
        let _g = super::super::super::test_lock();
        task::reset_all();
        let id = task::new_id();
        let _abort = task::register(&id, "demo", "why", None, None, None);
        {
            let run = task::find(&id).unwrap();
            let mut log = run.progress.lock().unwrap();
            // index 0 已结算（有 recordId），index 1 还在跑（无 recordId）
            log.push(serde_json::json!({
                "type": "workflow_agent", "index": 0, "label": "done a", "state": "done",
                "recordId": "rec0", "startedAt": 1, "lastProgressAt": 5
            }));
            log.push(serde_json::json!({
                "type": "workflow_agent", "index": 1, "label": "running b", "state": "start"
            }));
            // index 2：正在跑但**已经登记了 recordId**（spawn 时回传）→ 应 advertised `c`
            log.push(serde_json::json!({
                "type": "workflow_agent", "index": 2, "label": "running c", "state": "progress",
                "recordId": "rec2"
            }));
        }

        let keys_for = |state: InspectorState| -> Vec<String> {
            let run = task::find(&id).unwrap();
            footer(&run, state).into_iter().map(|(k, _)| k).collect()
        };

        // 已结算的 agent：可 `c`，且运行未终结 → 可 `x`
        let settled = keys_for(InspectorState {
            selected: 0,
            flat: true,
        });
        assert!(settled.contains(&"c".to_string()), "{settled:?}");
        assert!(settled.contains(&"x".to_string()), "{settled:?}");

        // 正在跑的 agent：没有 recordId → 不 advertised `c`
        let running = keys_for(InspectorState {
            selected: 1,
            flat: true,
        });
        assert!(
            !running.contains(&"c".to_string()),
            "跑着的 agent 打开不了，不该提示 c: {running:?}"
        );

        // 运行中但已有 recordId（spawn 时就登记）→ `c` 可用
        let running_openable = keys_for(InspectorState {
            selected: 2,
            flat: true,
        });
        assert!(
            running_openable.contains(&"c".to_string()),
            "登记了 recordId 的运行中 agent 应该能 c: {running_openable:?}"
        );

        // 阶段行：skip/retry/open 都不可用（均要求选中 agent 行）
        let phase = keys_for(InspectorState {
            selected: 0,
            flat: false,
        });
        assert!(!phase.contains(&"c".to_string()), "{phase:?}");
        assert!(!phase.contains(&"s".to_string()), "{phase:?}");
        assert!(!phase.contains(&"r".to_string()), "{phase:?}");

        // 终结后：不再 advertised `x`/`p`，但已结算的 `c` 仍在
        task::finish(&id, task::RunStatus::Completed, None, None);
        let done = keys_for(InspectorState {
            selected: 0,
            flat: true,
        });
        assert!(!done.contains(&"x".to_string()), "{done:?}");
        assert!(!done.contains(&"p".to_string()), "{done:?}");
        assert!(done.contains(&"c".to_string()), "{done:?}");
        // 运行已终结的 agent 停不了、也 skip/retry 不了 → 这些键都不该再 advertised
        assert!(!done.contains(&"s".to_string()), "{done:?}");
        assert!(!done.contains(&"r".to_string()), "{done:?}");

        task::reset_all();
    }

    #[test]
    fn rows_and_selection_follow_the_view_mode() {
        let _g = super::super::super::test_lock();
        task::reset_all();
        let id = task::new_id();
        let _abort = task::register(&id, "demo", "d", None, None, None);
        let run = task::find(&id).unwrap();
        run.progress.lock().unwrap().push(serde_json::json!({
            "type": "workflow_agent", "index": 7, "label": "a", "state": "done"
        }));
        let run = task::find(&id).unwrap();
        // 无阶段：折叠成一个 "Agents" 组
        let phase_rows = rows(&run, InspectorState::default());
        assert!(matches!(phase_rows[0], Row::Phase { .. }), "{phase_rows:?}");
        assert_eq!(
            selected_agent(&run, InspectorState::default()),
            None,
            "选中的是阶段行"
        );
        let agent_rows = rows(
            &run,
            InspectorState {
                selected: 0,
                flat: true,
            },
        );
        assert_eq!(agent_rows.len(), 1);
        assert_eq!(
            selected_agent(
                &run,
                InspectorState {
                    selected: 0,
                    flat: true
                }
            ),
            Some(7)
        );
        task::reset_all();
    }

    #[test]
    fn wrap_capped_folds_to_width_and_caps_lines() {
        // 短文本：一行，完整保留
        assert_eq!(
            wrap_capped("hello world", 40, MAX_PROMPT_LINES),
            vec!["hello world".to_string()]
        );

        // 换行当段落边界（空行丢弃）
        assert_eq!(
            wrap_capped("a\n\nb", 40, MAX_PROMPT_LINES),
            vec!["a".to_string(), "b".to_string()]
        );

        // 超长文本：封顶 10 行，末行标 `...`，且每行不超宽
        let long = "word ".repeat(200);
        let lines = wrap_capped(&long, 20, MAX_PROMPT_LINES);
        assert_eq!(lines.len(), MAX_PROMPT_LINES);
        assert!(lines.last().unwrap().ends_with("..."), "{lines:?}");
        for l in &lines {
            assert!(
                crate::utils::display::display_width(l) <= 20,
                "折行后不超宽度: {l:?}"
            );
        }
    }

    /// 检查器右栏的 prompt 要折行展示（不再单行截断），
    /// 最多 [`MAX_PROMPT_LINES`] 行，超出时末行以 `...` 收尾。
    #[test]
    fn view_wraps_long_prompt_and_caps_at_ten_lines() {
        let _g = super::super::super::test_lock();
        task::reset_all();
        let id = task::new_id();
        let _abort = task::register(&id, "demo", "why", None, None, None);
        let long_prompt: String = (0..80).map(|i| format!("word{i:02} ")).collect();
        {
            let run = task::find(&id).unwrap();
            let mut log = run.progress.lock().unwrap();
            log.push(serde_json::json!({
                "type": "workflow_agent", "index": 0, "label": "a", "state": "done",
                "prompt": long_prompt, "promptPreview": "word00 word01"
            }));
        }

        let run = task::find(&id).unwrap();
        let v = view(
            &run,
            InspectorState {
                selected: 0,
                flat: true,
            },
            60,
        );
        let right: Vec<String> = v
            .lines
            .iter()
            .map(text)
            .filter_map(|l| l.split("│ ").nth(1).map(str::to_string))
            .collect();

        let start = right
            .iter()
            .position(|r| r.starts_with("prompt:"))
            .expect("右栏应显示 prompt");
        // prompt 是这条 agent 详情的最后一段：一直取到第一个空行（后续行是左栏填充）。
        let prompt_lines: Vec<&String> = right[start..]
            .iter()
            .take_while(|r| !r.is_empty())
            .collect();

        assert_eq!(
            prompt_lines[0], "prompt:",
            "多行 prompt 标签独占一行: {right:?}"
        );
        let content = &prompt_lines[1..];
        assert!(
            content[0].starts_with("  word00"),
            "正文缩进 2 格（距 `│` 3 格）: {right:?}"
        );
        assert!(content.len() > 1, "长 prompt 应折成多行: {right:?}");
        // `MAX_PROMPT_LINES` 限制的是 prompt **正文**行数（标签另占一行）。
        assert!(
            content.len() <= MAX_PROMPT_LINES,
            "prompt 最多 {} 行，实际 {}: {right:?}",
            MAX_PROMPT_LINES,
            content.len()
        );
        assert!(
            content.last().unwrap().ends_with("..."),
            "超过 10 行应在末尾标 `...`: {right:?}"
        );
        // 右栏宽度 = 60 - 24(left) - 3(分隔) = 33
        assert!(
            prompt_lines.iter().all(|l| l.chars().count() <= 33),
            "prompt 每行不超右栏宽度: {prompt_lines:?}"
        );
        task::reset_all();
    }

    /// 短 prompt（单行）保持 `prompt: <正文>` 同行。
    #[test]
    fn view_keeps_short_prompt_inline() {
        let _g = super::super::super::test_lock();
        task::reset_all();
        let id = task::new_id();
        let _abort = task::register(&id, "demo", "why", None, None, None);
        {
            let run = task::find(&id).unwrap();
            let mut log = run.progress.lock().unwrap();
            log.push(serde_json::json!({
                "type": "workflow_agent", "index": 0, "label": "a", "state": "done",
                "prompt": "count the files"
            }));
        }

        let run = task::find(&id).unwrap();
        let v = view(
            &run,
            InspectorState {
                selected: 0,
                flat: true,
            },
            100,
        );
        let lines: Vec<String> = v.lines.iter().map(text).collect();
        assert!(
            lines
                .iter()
                .any(|l| l.contains("│ prompt: count the files")),
            "短 prompt 应与标签同行: {lines:?}"
        );
        task::reset_all();
    }

    /// 左栏文本含宽字符（CJK）时，折叠行的 `│` 仍必须与其它行对齐。
    #[test]
    fn columns_align_with_wide_left_text() {
        let _g = super::super::super::test_lock();
        task::reset_all();
        let id = task::new_id();
        let _abort = task::register(&id, "demo", "why", None, None, None);
        {
            let run = task::find(&id).unwrap();
            let mut log = run.progress.lock().unwrap();
            log.push(
                serde_json::json!({ "type": "workflow_phase", "index": 0, "title": "扫描阶段" }),
            );
            log.push(serde_json::json!({
                "type": "workflow_agent", "index": 0, "label": "扫描 A", "state": "done",
                "phaseIndex": 0, "startedAt": 1, "lastProgressAt": 2
            }));
            log.push(serde_json::json!({
                "type": "workflow_agent", "index": 1, "label": "扫描 B 的更长标签", "state": "done",
                "phaseIndex": 0, "startedAt": 3, "lastProgressAt": 4
            }));
        }

        let run = task::find(&id).unwrap();
        let v = view(&run, InspectorState::default(), 100);
        let cols: Vec<Option<usize>> = v
            .lines
            .iter()
            .map(text)
            .map(|l| {
                l.find('│')
                    .map(|i| crate::utils::display::display_width(&l[..i]))
            })
            .collect();
        let dividers: Vec<usize> = cols.into_iter().flatten().collect();
        assert!(
            dividers.len() >= 3,
            "应有多个带分隔线的正文行: {dividers:?}"
        );
        assert!(
            dividers.windows(2).all(|w| w[0] == w[1]),
            "所有行的 `│` 应对齐到同一列: {dividers:?}"
        );
        task::reset_all();
    }
}
