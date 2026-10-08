//! 任务工具的执行体
//!
//! 模型可见的工具名、参数名、状态词与输出文案——刻意镜像 Claude Code 的调用约定。

use crate::{
    core::tools::{ToolError, ToolResult},
    extensions::{
        tasks::{
            store::TaskStore,
            types::{Metadata, TaskStatus, TaskStatusOrDeleted, TaskUpdateFields},
        },
        util::{numeric_id, opt_str, opt_string_array, req_str},
    },
};
use serde_json::Value;
use std::cmp::Ordering;

/// 把文案包成纯文本 `ToolResult`（工具返回给模型的输出统一走这里）。
fn text(msg: impl Into<String>) -> ToolResult {
    ToolResult::text(msg)
}

/// 从工具参数里取 `metadata` 对象；缺失或不是对象时返回 None（调用方按空元数据处理）。
fn opt_metadata(args: &Value) -> Option<Metadata> {
    args.get("metadata").and_then(Value::as_object).cloned()
}

/// `TaskCreate`
pub fn task_create(args: &Value, store: &mut TaskStore) -> Result<ToolResult, ToolError> {
    let subject = req_str(args, "subject")?;
    let description = req_str(args, "description")?;
    let active_form = opt_str(args, "activeForm");
    let agent_type = opt_str(args, "agentType");

    let mut meta = opt_metadata(args).unwrap_or_default();
    if let Some(agent_type) = agent_type {
        meta.insert("agentType".to_string(), Value::String(agent_type));
    }
    let metadata = (!meta.is_empty()).then_some(meta);

    let task = store
        .create(subject, description, active_form, metadata)
        .map_err(ToolError)?;
    Ok(text(format!(
        "Task #{} created successfully: {}",
        task.id, task.subject
    )))
}

/// `TaskList`
pub fn task_list(_args: &Value, store: &mut TaskStore) -> Result<ToolResult, ToolError> {
    let tasks = store.list(None);
    if tasks.is_empty() {
        return Ok(text("No tasks found"));
    }

    let mut sorted = tasks;
    sorted.sort_by(|a, b| {
        a.status.cmp(&b.status).then_with(|| {
            numeric_id(&a.id)
                .partial_cmp(&numeric_id(&b.id))
                .unwrap_or(Ordering::Equal)
        })
    });

    let mut lines = Vec::with_capacity(sorted.len());
    for task in sorted {
        let mut line = format!("#{} [{}] {}", task.id, task.status.as_str(), task.subject);
        if let Some(owner) = &task.owner {
            line.push_str(&format!(" ({owner})"));
        }

        // 只显示未完成的阻塞者
        let mut open: Vec<String> = Vec::new();
        for bid in &task.blocked_by {
            if let Some(blocker) = store.get(bid)
                && blocker.status != TaskStatus::Completed
            {
                open.push(format!("#{bid}"));
            }
        }

        if !open.is_empty() {
            line.push_str(&format!(" [blocked by {}]", open.join(", ")));
        }
        lines.push(line);
    }
    Ok(text(lines.join("\n")))
}

/// `TaskGet`
pub fn task_get(args: &Value, store: &mut TaskStore) -> Result<ToolResult, ToolError> {
    let task_id = req_str(args, "taskId")?;
    let Some(task) = store.get(&task_id) else {
        return Ok(text("Task not found"));
    };

    // 模型可能把 JSON 里的 \n 双重转义了：还原成真实换行
    let desc = task.description.replace("\\n", "\n");

    let mut lines = vec![
        format!("Task #{}: {}", task.id, task.subject),
        format!("Status: {}", task.status.as_str()),
    ];

    if let Some(owner) = &task.owner {
        lines.push(format!("Owner: {owner}"));
    }
    lines.push(format!("Description: {desc}"));

    let open: Vec<String> = task
        .blocked_by
        .iter()
        .filter(|bid| {
            store
                .get(bid)
                .map(|b| b.status != TaskStatus::Completed)
                .unwrap_or(false)
        })
        .map(|id| format!("#{id}"))
        .collect();

    if !open.is_empty() {
        lines.push(format!("Blocked by: {}", open.join(", ")));
    }

    if !task.blocks.is_empty() {
        let blocks: Vec<String> = task.blocks.iter().map(|id| format!("#{id}")).collect();
        lines.push(format!("Blocks: {}", blocks.join(", ")));
    }

    if !task.metadata.is_empty() {
        lines.push(format!(
            "Metadata: {}",
            serde_json::to_string(&task.metadata).unwrap_or_default()
        ));
    }
    Ok(text(lines.join("\n")))
}

/// `TaskUpdate`
pub fn task_update(args: &Value, store: &mut TaskStore) -> Result<ToolResult, ToolError> {
    let task_id = req_str(args, "taskId")?;

    let status = match opt_str(args, "status") {
        Some(s) => Some(
            TaskStatusOrDeleted::parse(&s)
                .ok_or_else(|| ToolError(format!("invalid status: {s}")))?,
        ),
        None => None,
    };

    let fields = TaskUpdateFields {
        status,
        subject: opt_str(args, "subject"),
        description: opt_str(args, "description"),
        active_form: opt_str(args, "activeForm"),
        owner: opt_str(args, "owner"),
        metadata: opt_metadata(args),
        add_blocks: opt_string_array(args, "addBlocks"),
        add_blocked_by: opt_string_array(args, "addBlockedBy"),
    };

    let outcome = store.update(&task_id, fields).map_err(ToolError)?;
    if outcome.changed_fields.is_empty() && outcome.task.is_none() {
        return Ok(text(format!("Task #{task_id} not found")));
    }

    let mut msg = format!(
        "Updated task #{task_id} {}",
        outcome.changed_fields.join(", ")
    );

    if !outcome.warnings.is_empty() {
        msg.push_str(&format!(" (warning: {})", outcome.warnings.join("; ")));
    }

    Ok(text(msg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn store() -> TaskStore {
        TaskStore::new(None)
    }

    #[test]
    fn create_then_list_and_get() {
        let mut s = store();
        let r = task_create(
            &json!({ "subject": "Fix bug", "description": "Details", "agentType": "Explore" }),
            &mut s,
        )
        .unwrap();
        assert!(r.text.contains("Task #1 created successfully: Fix bug"));

        let list = task_list(&json!({}), &mut s).unwrap();
        assert!(list.text.contains("#1 [pending] Fix bug"));

        let get = task_get(&json!({ "taskId": "1" }), &mut s).unwrap();
        assert!(get.text.contains("Task #1: Fix bug"));
        assert!(get.text.contains("Metadata:"), "agentType 进了 metadata");
    }

    #[test]
    fn list_hides_completed_blockers() {
        let mut s = store();
        task_create(&json!({ "subject": "A", "description": "d" }), &mut s).unwrap();
        task_create(&json!({ "subject": "B", "description": "d" }), &mut s).unwrap();
        task_update(&json!({ "taskId": "2", "addBlockedBy": ["1"] }), &mut s).unwrap();
        assert!(
            task_list(&json!({}), &mut s)
                .unwrap()
                .text
                .contains("blocked by #1")
        );
        task_update(&json!({ "taskId": "1", "status": "completed" }), &mut s).unwrap();
        assert!(
            !task_list(&json!({}), &mut s)
                .unwrap()
                .text
                .contains("blocked by")
        );
    }

    #[test]
    fn update_reports_fields_and_warnings() {
        let mut s = store();
        task_create(&json!({ "subject": "A", "description": "d" }), &mut s).unwrap();
        let r = task_update(
            &json!({ "taskId": "1", "status": "in_progress", "addBlockedBy": ["9"] }),
            &mut s,
        )
        .unwrap();
        assert!(r.text.contains("Updated task #1 status, blockedBy"));
        assert!(r.text.contains("warning: #9 does not exist"));
    }

    #[test]
    fn unknown_task_messages() {
        let mut s = store();
        assert_eq!(
            task_get(&json!({ "taskId": "1" }), &mut s).unwrap().text,
            "Task not found"
        );
        assert_eq!(
            task_update(&json!({ "taskId": "1", "status": "completed" }), &mut s)
                .unwrap()
                .text,
            "Task #1 not found"
        );
        assert_eq!(
            task_list(&json!({}), &mut s).unwrap().text,
            "No tasks found"
        );
    }

    #[test]
    fn deleted_status_removes_task() {
        let mut s = store();
        task_create(&json!({ "subject": "A", "description": "d" }), &mut s).unwrap();
        let r = task_update(&json!({ "taskId": "1", "status": "deleted" }), &mut s).unwrap();
        assert!(r.text.contains("Updated task #1 deleted"));
        assert!(s.get("1").is_none());
    }
}
