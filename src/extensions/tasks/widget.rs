//! 常驻任务 widget
//!
//! 展示风格对齐 Claude Code 的任务列表：
//! - `✔` 已完成；
//! - `◼` 进行中（未在执行）；
//! - `◻` 待办；
//! - `✳`/`✽` 正在执行（star spinner + `activeForm` 文本 + 耗时 + token 数）。

use super::{
    auto_hide::AutoHideManager,
    config::TasksConfig,
    store::TaskStore,
    types::{Task, TaskStatus},
};
use crate::{
    core::extensions::{DockLine, DockSpan},
    extensions::util::{dock_plain, dock_span, format_tokens},
    utils::{
        glyphs::{
            DEF_BLOCKED, DEF_COMPLETED, DEF_HEADER, DEF_IN_PROGRESS, DEF_INPUT_TOKENS,
            DEF_OUTPUT_TOKENS, DEF_OVERFLOW, DEF_PENDING, DEF_SPINNER, DEF_STATS_SEPARATOR,
            DEF_TRAILING_ELLIPSIS,
        },
        time::{format_duration, now_ms},
    },
};
use std::collections::BTreeMap;

/// 单个任务的运行期指标（耗时、token 用量）。
#[derive(Debug, Clone, Default)]
pub struct Metrics {
    /// 任务进入活跃（执行）状态的时间（毫秒时间戳）。
    pub started_at: u64,
    /// 累计输入 token 数。
    pub input: u64,
    /// 累计输出 token 数。
    pub output: u64,
}

/// widget 运行态：哪些任务正在被执行 + 各自指标。
#[derive(Debug, Default)]
pub struct WidgetState {
    /// 正在执行的任务 id → 运行期指标。
    active: BTreeMap<String, Metrics>,
}

impl WidgetState {
    /// 标记任务为活跃 / 非活跃；首次标记活跃时记录开始时间，取消时删除其指标。
    pub fn set_active(&mut self, task_id: &str, active: bool) {
        if active {
            self.active
                .entry(task_id.to_string())
                .or_insert_with(|| Metrics {
                    started_at: now_ms(),
                    ..Default::default()
                });
        } else {
            self.active.remove(task_id);
        }
    }

    /// 该任务当前是否被标记为活跃。
    pub fn is_active(&self, task_id: &str) -> bool {
        self.active.contains_key(task_id)
    }

    /// 是否存在任何活跃任务。
    pub fn has_active(&self) -> bool {
        !self.active.is_empty()
    }

    /// 清空所有活跃任务及其指标。
    pub fn reset(&mut self) {
        self.active.clear();
    }

    /// 把 token 用量分给所有当前活跃的任务
    pub fn add_usage(&mut self, input: u64, output: u64) {
        for m in self.active.values_mut() {
            m.input = m.input.saturating_add(input);
            m.output = m.output.saturating_add(output);
        }
    }

    /// 剔除已删除或不再 in_progress 的活跃项。
    pub fn prune(&mut self, store: &TaskStore) {
        let stale: Vec<String> = self
            .active
            .keys()
            .filter(|id| {
                store
                    .cached_get(id)
                    .map(|t| t.status != TaskStatus::InProgress)
                    .unwrap_or(true)
            })
            .cloned()
            .collect();

        for id in stale {
            self.active.remove(&id);
        }
    }
}

/// 构建停靠面板行。空列表返回空（TUI 自动隐藏面板）。
///
/// `auto_hide` 先按墙钟推进（隐藏整列表完成后的旧任务），再把被隐藏的已完成任务从列表中滤掉。
pub fn build_lines(
    store: &TaskStore,
    config: &TasksConfig,
    widget: &WidgetState,
    auto_hide: &mut AutoHideManager,
    frame: usize,
) -> Vec<DockLine> {
    let all = store.list_cached(config.sort_order());
    auto_hide.tick(&all, now_ms(), config.hide_completed_after_ms());

    let tasks: Vec<&Task> = all.iter().filter(|t| !auto_hide.hides(t)).collect();
    if tasks.is_empty() {
        return Vec::new();
    }

    let completed = tasks
        .iter()
        .filter(|t| t.status == TaskStatus::Completed)
        .count();
    let in_progress = tasks
        .iter()
        .filter(|t| t.status == TaskStatus::InProgress)
        .count();
    let pending = tasks
        .iter()
        .filter(|t| t.status == TaskStatus::Pending)
        .count();

    let mut parts: Vec<String> = Vec::new();
    if completed > 0 {
        parts.push(format!("{completed} done"));
    }
    if in_progress > 0 {
        parts.push(format!("{in_progress} in progress"));
    }
    if pending > 0 {
        parts.push(format!("{pending} open"));
    }
    let status_text = format!("{} tasks ({})", tasks.len(), parts.join(", "));

    let mut lines: Vec<DockLine> = Vec::new();
    lines.push(vec![
        dock_span("accent", "#8abeb7", DEF_HEADER),
        dock_span("accent", "#8abeb7", format!(" {status_text}")),
    ]);

    // 折叠只决定进列表的内容；可见上限逻辑随后照常作用在剩下的行上。
    let listed: Vec<&Task> = if config.collapse_completed() {
        tasks
            .iter()
            .filter(|t| t.status != TaskStatus::Completed)
            .copied()
            .collect()
    } else {
        tasks.clone()
    };

    let limit = config.max_visible() as usize;
    let hidden_at_top = config.hidden_at() == super::config::HiddenAt::Top;
    let visible: Vec<&Task> = if config.show_all() {
        listed.clone()
    } else if hidden_at_top {
        listed.iter().rev().take(limit).rev().copied().collect()
    } else {
        listed.iter().take(limit).copied().collect()
    };
    let hidden_count = listed.len() - visible.len();
    let overflow_line = (hidden_count > 0).then(|| {
        vec![dock_span(
            "dim",
            "#666666",
            format!("    {} and {hidden_count} more", DEF_OVERFLOW),
        )]
    });

    if hidden_at_top && let Some(line) = &overflow_line {
        lines.push(line.clone());
    }

    let spinner_frame = DEF_SPINNER[frame % DEF_SPINNER.len()];
    for task in &visible {
        let is_active = widget.is_active(&task.id) && task.status == TaskStatus::InProgress;

        let status_glyph: DockSpan = if is_active {
            dock_span("accent", "#8abeb7", spinner_frame)
        } else {
            match task.status {
                TaskStatus::Completed => dock_span("success", "#b5bd68", DEF_COMPLETED),
                TaskStatus::InProgress => dock_span("accent", "#8abeb7", DEF_IN_PROGRESS),
                TaskStatus::Pending => dock_plain(DEF_PENDING),
            }
        };

        let mut row: DockLine = vec![dock_plain("  "), status_glyph, dock_plain(" ")];

        let blocked_suffix = if task.status == TaskStatus::Pending && !task.blocked_by.is_empty() {
            let open: Vec<String> = task
                .blocked_by
                .iter()
                .filter(|bid| {
                    store
                        .cached_get(bid)
                        .map(|b| b.status != TaskStatus::Completed)
                        .unwrap_or(false)
                })
                .map(|id| format!("#{id}"))
                .collect();
            (!open.is_empty()).then(|| format!(" {} blocked by {}", DEF_BLOCKED, open.join(", ")))
        } else {
            None
        };

        if is_active {
            let form = task
                .active_form
                .clone()
                .unwrap_or_else(|| task.subject.clone());
            let agent_label = task
                .metadata
                .get("agentId")
                .and_then(|v| v.as_str())
                .map(|id| format!(" (agent {})", &id[..id.len().min(5)]))
                .unwrap_or_default();
            row.push(dock_span("dim", "#666666", format!("#{} ", task.id)));
            row.push(dock_span(
                "accent",
                "#8abeb7",
                format!("{form}{agent_label}{DEF_TRAILING_ELLIPSIS}"),
            ));
            if let Some(m) = widget.active.get(&task.id) {
                let elapsed = format_duration(now_ms().saturating_sub(m.started_at));
                let mut token_parts: Vec<String> = Vec::new();
                if m.input > 0 {
                    token_parts.push(format!("{} {}", DEF_INPUT_TOKENS, format_tokens(m.input)));
                }
                if m.output > 0 {
                    token_parts.push(format!("{} {}", DEF_OUTPUT_TOKENS, format_tokens(m.output)));
                }
                let stats = if token_parts.is_empty() {
                    format!(" ({elapsed})")
                } else {
                    format!(
                        " ({elapsed} {} {})",
                        DEF_STATS_SEPARATOR,
                        token_parts.join(" ")
                    )
                };
                row.push(dock_span("dim", "#666666", stats));
            }
        } else if task.status == TaskStatus::Completed {
            row.push(dock_span(
                "success",
                "#b5bd68",
                format!("#{} {}", task.id, task.subject),
            ));
        } else {
            row.push(dock_span("dim", "#666666", format!("#{} ", task.id)));
            row.push(dock_plain(task.subject.clone()));
            if task.status == TaskStatus::InProgress
                && let Some(agent_id) = task.metadata.get("agentId").and_then(|v| v.as_str())
            {
                row.push(dock_span(
                    "dim",
                    "#666666",
                    format!(" (agent {})", &agent_id[..agent_id.len().min(5)]),
                ));
            }
        }

        if let Some(suffix) = blocked_suffix {
            row.push(dock_span("dim", "#666666", suffix));
        }
        lines.push(row);
    }

    if !hidden_at_top && let Some(line) = &overflow_line {
        lines.push(line.clone());
    }
    if config.collapse_completed() && completed > 0 {
        lines.push(vec![
            dock_plain("  "),
            dock_span("success", "#b5bd68", DEF_COMPLETED),
            dock_span("dim", "#666666", format!(" {completed} completed")),
        ]);
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::tasks::types::TaskUpdateFields;

    fn store() -> TaskStore {
        TaskStore::new(None)
    }

    fn line_text(line: &DockLine) -> String {
        line.iter().map(|s| s.text.clone()).collect()
    }

    #[test]
    fn empty_store_has_no_lines() {
        let s = store();
        assert!(
            build_lines(
                &s,
                &TasksConfig::default(),
                &WidgetState::default(),
                &mut AutoHideManager::default(),
                0
            )
            .is_empty()
        );
    }

    #[test]
    fn header_and_rows() {
        let mut s = store();
        s.create("A".into(), "d".into(), None, None).unwrap();
        s.create("B".into(), "d".into(), None, None).unwrap();
        let lines = build_lines(
            &s,
            &TasksConfig::default(),
            &WidgetState::default(),
            &mut AutoHideManager::default(),
            0,
        );
        assert_eq!(line_text(&lines[0]), "● 2 tasks (2 open)");
        assert!(line_text(&lines[1]).contains("◻ #1 A"));
    }

    #[test]
    fn hidden_at_top_puts_overflow_first() {
        let mut s = store();
        for i in 0..5 {
            s.create(format!("t{i}"), "d".into(), None, None).unwrap();
        }
        let mut cfg = TasksConfig::default();
        cfg.max_visible = Some(2);
        cfg.hidden_at = Some(super::super::config::HiddenAt::Top);
        let lines = build_lines(
            &s,
            &cfg,
            &WidgetState::default(),
            &mut AutoHideManager::default(),
            0,
        );
        assert!(line_text(&lines[1]).contains("and 3 more"));
        // 保留的是靠后的任务（4、5）
        assert!(line_text(&lines[2]).contains("#4"));
        assert!(line_text(&lines[3]).contains("#5"));
    }

    #[test]
    fn collapse_completed_hides_rows_and_summarizes() {
        let mut s = store();
        s.create("A".into(), "d".into(), None, None).unwrap();
        s.create("B".into(), "d".into(), None, None).unwrap();
        s.update(
            "1",
            TaskUpdateFields {
                status: Some(super::super::types::TaskStatusOrDeleted::Status(
                    TaskStatus::Completed,
                )),
                ..Default::default()
            },
        )
        .unwrap();
        let mut cfg = TasksConfig::default();
        cfg.collapse_completed = Some(true);
        let lines = build_lines(
            &s,
            &cfg,
            &WidgetState::default(),
            &mut AutoHideManager::default(),
            0,
        );
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert!(!texts.iter().any(|t| t.contains("#1 A")), "已完成行被折叠");
        assert!(texts.iter().any(|t| t.contains("1 completed")));
    }

    #[test]
    fn active_task_shows_spinner_and_stats() {
        let mut s = store();
        s.create("Work".into(), "d".into(), Some("Working".into()), None)
            .unwrap();
        s.update(
            "1",
            TaskUpdateFields {
                status: Some(super::super::types::TaskStatusOrDeleted::Status(
                    TaskStatus::InProgress,
                )),
                ..Default::default()
            },
        )
        .unwrap();
        let mut w = WidgetState::default();
        w.set_active("1", true);
        w.add_usage(4100, 1200);
        let lines = build_lines(
            &s,
            &TasksConfig::default(),
            &w,
            &mut AutoHideManager::default(),
            0,
        );
        let row = line_text(&lines[1]);
        assert!(row.contains("Working"), "显示 activeForm: {row}");
        assert!(row.contains("4.1K") && row.contains("1.2K"), "token: {row}");
    }

    #[test]
    fn auto_hidden_completed_tasks_drop_out_of_widget() {
        let mut s = store();
        s.create("A".into(), "d".into(), None, None).unwrap();
        s.update(
            "1",
            TaskUpdateFields {
                status: Some(super::super::types::TaskStatusOrDeleted::Status(
                    TaskStatus::Completed,
                )),
                ..Default::default()
            },
        )
        .unwrap();

        // 用一个远大于任务 updated_at 的时刻触发隐藏。
        let mut m = AutoHideManager::default();
        let tasks = s.list_cached(None);
        let far = 10_000_000_000_000;
        m.tick(&tasks, far, Some(0));
        m.tick(&tasks, far, Some(0));
        assert!(m.hides(&tasks[0]));

        let lines = build_lines(
            &s,
            &TasksConfig::default(),
            &WidgetState::default(),
            &mut m,
            0,
        );
        assert!(lines.is_empty(), "整列表被墙钟隐藏后 dock 不再占位");
    }

    #[test]
    fn blocked_suffix_only_for_open_blockers() {
        let mut s = store();
        s.create("A".into(), "d".into(), None, None).unwrap();
        s.create("B".into(), "d".into(), None, None).unwrap();
        s.update(
            "2",
            TaskUpdateFields {
                add_blocked_by: Some(vec!["1".into()]),
                ..Default::default()
            },
        )
        .unwrap();
        let lines = build_lines(
            &s,
            &TasksConfig::default(),
            &WidgetState::default(),
            &mut AutoHideManager::default(),
            0,
        );
        assert!(line_text(&lines[2]).contains("blocked by #1"));
        s.update(
            "1",
            TaskUpdateFields {
                status: Some(super::super::types::TaskStatusOrDeleted::Status(
                    TaskStatus::Completed,
                )),
                ..Default::default()
            },
        )
        .unwrap();
        let lines = build_lines(
            &s,
            &TasksConfig::default(),
            &WidgetState::default(),
            &mut AutoHideManager::default(),
            0,
        );
        assert!(!line_text(&lines[2]).contains("blocked by"));
    }
}
