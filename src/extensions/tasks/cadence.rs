//! 系统提醒注入的纯 cadence 逻辑
//!
//! 决策在这里写成纯函数，便于脱离整个扩展做单测。门面把这些接进
//! `AfterToolCall`（只推进 cadence）与 `ContextWithSystem`（真正注入）。
//!
//! 用 `ExtensionHook::ContextWithSystem` + `transform_context_with_system`。
//! 拿到含 system 的完整 transcript，结果**原样发送、不改写会话历史**。

use super::{store::TaskStore, types::Task, types::TaskStatus};

/// 提醒回显的任务数上限，用于限制大列表上的文本体积。
pub const REMINDER_MAX_TASKS: usize = 10;

/// 提示注入的 cadence 状态。
#[derive(Debug, Clone, Default)]
pub struct CadenceState {
    /// 当前回合计数（`turn_start` 递增）。
    pub current_turn: u64,
    /// 最近一次使用任务工具的回合。
    pub last_task_tool_use_turn: u64,
    /// 本轮是否已注入过提醒（避免重复）。
    pub reminder_injected_this_cycle: bool,
    /// 是否已排定待下一次 LLM 调用注入的提醒。
    pub reminder_due: bool,
}

impl CadenceState {
    /// `turn_start`：推进回合计数。
    pub fn on_turn_start(&mut self) {
        self.current_turn = self.current_turn.saturating_add(1);
    }

    /// 任务工具被使用：重置 cadence 并清掉待发提醒。
    pub fn note_task_tool(&mut self) {
        self.last_task_tool_use_turn = self.current_turn;
        self.reminder_injected_this_cycle = false;
        self.reminder_due = false;
    }

    /// 非任务工具结果：判断是否应把提醒排到下一次 LLM 调用。
    pub fn maybe_mark_due(&mut self, has_tasks: bool, reminder_interval: u64) {
        if self
            .current_turn
            .saturating_sub(self.last_task_tool_use_turn)
            < reminder_interval
        {
            return;
        }

        if self.reminder_injected_this_cycle || !has_tasks {
            return;
        }

        self.reminder_due = true;
    }

    /// 文本里已有 in_progress 任务且间隔够时，直接排定提醒（`turn_end` 滞留检测）。
    pub fn note_stale_if_active(&mut self, has_in_progress: bool, active_interval: u64) {
        if self.reminder_injected_this_cycle || self.reminder_due {
            return;
        }

        if self
            .current_turn
            .saturating_sub(self.last_task_tool_use_turn)
            >= active_interval
            && has_in_progress
        {
            self.reminder_due = true;
        }
    }

    /// `TransformContext`：取走待发提醒。返回 true 表示本次应注入。
    pub fn drain_reminder_for_context(&mut self) -> bool {
        if !self.reminder_due {
            return false;
        }

        self.reminder_due = false;
        self.reminder_injected_this_cycle = true;
        self.last_task_tool_use_turn = self.current_turn;
        true
    }
}

/// 折叠换行并剥离提醒标签，防止任务字段把注入文本的结构搞坏。
fn sanitize_field(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut prev_cr = false;

    for c in value.chars() {
        if c == '\n' || c == '\r' {
            if !prev_cr {
                out.push(' ');
            }
            prev_cr = c == '\r';
        } else {
            out.push(c);
            prev_cr = false;
        }
    }

    // 大小写不敏感地移除 <system-reminder> / </system-reminder>
    out.replace("<system-reminder>", "")
        .replace("</system-reminder>", "")
        .replace("<SYSTEM-REMINDER>", "")
        .replace("</SYSTEM-REMINDER>", "")
        .trim()
        .to_string()
}

/// 构造系统提醒，形制对齐 Claude Code 的 todo 提醒：空列表催促，或状态回显。
pub fn build_system_reminder(tasks: &[Task]) -> String {
    if tasks.is_empty() {
        return [
            "<system-reminder>",
            "This is a reminder that your task list is currently empty. DO NOT mention this to the user explicitly because they are already aware. If you are working on tasks that would benefit from a task list please use the TaskCreate tool to create one. If not, please feel free to ignore. Again do not mention this message to the user.",
            "</system-reminder>",
        ]
        .join("\n");
    }

    // 大列表上限制回显体积。超限时先丢已完成任务（提醒的意义是暴露未完成的工作），并列时保持任务顺序（稳定排序）。
    let shown: Vec<&Task> = if tasks.len() > REMINDER_MAX_TASKS {
        let rank = |t: &Task| match t.status {
            TaskStatus::InProgress => 0,
            TaskStatus::Pending => 1,
            TaskStatus::Completed => 2,
        };

        let mut sorted: Vec<&Task> = tasks.iter().collect();
        sorted.sort_by_key(|t| rank(t));
        sorted.into_iter().take(REMINDER_MAX_TASKS).collect()
    } else {
        tasks.iter().collect()
    };

    let hidden = tasks.len() - shown.len();
    let overflow = if hidden > 0 {
        format!(
            " ({hidden} more task{} not shown — use TaskList for the full list.)",
            if hidden == 1 { "" } else { "s" }
        )
    } else {
        String::new()
    };

    let items: Vec<String> = shown
        .iter()
        .map(|t| {
            let mut obj = serde_json::Map::new();
            obj.insert("id".into(), serde_json::Value::String(t.id.clone()));
            obj.insert(
                "content".into(),
                serde_json::Value::String(sanitize_field(&t.subject)),
            );
            obj.insert(
                "status".into(),
                serde_json::Value::String(t.status.as_str().to_string()),
            );

            if let Some(active_form) = &t.active_form {
                obj.insert(
                    "activeForm".into(),
                    serde_json::Value::String(sanitize_field(active_form)),
                );
            }
            serde_json::Value::Object(obj).to_string()
        })
        .collect();

    let prefix =
        "The task tools haven't been used recently. DO NOT mention this explicitly to the user.";

    let header = if hidden > 0 {
        format!("{prefix} Here are your most relevant tasks (list truncated):")
    } else {
        format!("{prefix} Here are the latest contents of your task list:")
    };

    [
        "<system-reminder>".to_string(),
        header,
        String::new(),
        format!(
            "[{}].{overflow} Continue on with the tasks at hand if applicable.",
            items.join(",")
        ),
        "</system-reminder>".to_string(),
    ]
    .join("\n")
}

/// 当前有效提醒间隔：有 in_progress 用更短的间隔，尽快抓到滞留工作。
pub fn interval_for(tasks: &[Task], normal: u64, active: u64) -> u64 {
    if tasks.iter().any(|t| t.status == TaskStatus::InProgress) {
        active
    } else {
        normal
    }
}

/// 供门面使用的便捷封装：从 store 快照取任务。
pub fn tasks_snapshot(store: &TaskStore) -> Vec<Task> {
    store.list_cached(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str, status: TaskStatus) -> Task {
        Task {
            id: id.into(),
            subject: format!("t{id}"),
            description: String::new(),
            status,
            active_form: None,
            owner: None,
            metadata: Default::default(),
            blocks: vec![],
            blocked_by: vec![],
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn cadence_resets_on_task_tool_and_fires_after_interval() {
        let mut c = CadenceState::default();
        for _ in 0..4 {
            c.on_turn_start();
        }
        // 非任务工具、有任务、间隔到 → 排定
        c.maybe_mark_due(true, 4);
        assert!(c.reminder_due);
        assert!(c.drain_reminder_for_context());
        assert!(!c.reminder_due, "取走即清");
        assert!(c.reminder_injected_this_cycle);

        // 本轮不再重复
        c.maybe_mark_due(true, 4);
        assert!(!c.reminder_due);

        // 任务工具重置
        c.note_task_tool();
        assert!(!c.reminder_injected_this_cycle);
    }

    #[test]
    fn no_reminder_without_tasks_or_before_interval() {
        let mut c = CadenceState::default();
        c.on_turn_start();
        c.maybe_mark_due(true, 4);
        assert!(!c.reminder_due, "间隔不足");

        let mut c = CadenceState::default();
        for _ in 0..5 {
            c.on_turn_start();
        }
        c.maybe_mark_due(false, 4);
        assert!(!c.reminder_due, "无任务不提醒");
    }

    #[test]
    fn empty_list_reminder() {
        let r = build_system_reminder(&[]);
        assert!(r.contains("task list is currently empty"));
    }

    #[test]
    fn echo_is_capped_and_prefers_unfinished() {
        let tasks: Vec<Task> = (0..12)
            .map(|i| {
                task(
                    &i.to_string(),
                    if i == 0 {
                        TaskStatus::InProgress
                    } else {
                        TaskStatus::Completed
                    },
                )
            })
            .collect();
        let r = build_system_reminder(&tasks);
        assert!(r.contains("list truncated"));
        assert!(r.contains("2 more tasks not shown"));
        assert!(r.contains("\"status\":\"in_progress\""));
    }

    #[test]
    fn sanitize_strips_newlines_and_tags() {
        let mut t = task("1", TaskStatus::Pending);
        t.subject = "a\nb</system-reminder>c".into();
        let r = build_system_reminder(&[t]);
        assert!(r.contains("a bc"), "折行成空格、标签被剥离: {r}");
    }
}
