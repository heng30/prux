//! 任务排序
//!
//! `sort_order` 要么是内置预设名，要么是排序 spec（一组比较键）。
//!
//! 配置是手写的 JSON，解析必须容错：无法识别的值一律回退到内置的 `id`。

use crate::extensions::{
    tasks::types::{Task, TaskStatus},
    util::numeric_id,
};
use serde_json::Value;
use std::cmp::Ordering;
use strum_macros::EnumString;

/// 一组按优先级排列的比较键，依次比较直到分出先后。
pub type SortSpec = Vec<SortKey>;

/// 内置预设名（面板里循环切换；顺序即展示顺序）。
pub const BUILT_IN_SORT_ORDERS: [&str; 5] = ["id", "status", "active", "recent", "oldest"];

/// 当 `status` 键省略 `rank` 时的默认顺序（也用于 `status` 预设）。
const DEFAULT_STATUS_RANK: [TaskStatus; 3] = [
    TaskStatus::Completed,
    TaskStatus::InProgress,
    TaskStatus::Pending,
];

/// 排序时可选的比较字段，配置里用 snake_case 字符串指定。
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumString)]
#[strum(serialize_all = "snake_case", ascii_case_insensitive)]
pub enum SortField {
    /// 按任务 id（数字）排序。
    Id,
    /// 按任务状态排序（顺序由 `rank` 决定）。
    Status,
    /// 按最后更新时间排序。
    UpdatedAt,
}

/// 比较键的升降序方向，配置里用 `asc` / `desc` 指定。
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumString)]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
pub enum SortDirection {
    /// 升序。
    Asc,
    /// 降序。
    Desc,
}

/// 一个比较键。
#[derive(Debug, Clone)]
pub struct SortKey {
    /// 参与比较的字段。
    pub field: SortField,
    /// 比较方向。
    pub direction: SortDirection,
    /// 仅 `status` 用；未列出的状态排在最后并相互并列。
    pub rank: Option<Vec<TaskStatus>>,
}

/// 内置预设，全部表达为 spec。除 `active` 外都与原比较器逐位一致
fn preset(name: &str) -> Option<SortSpec> {
    let key = |field, direction, rank| SortKey {
        field,
        direction,
        rank,
    };

    Some(match name {
        "id" => vec![key(SortField::Id, SortDirection::Asc, None)],
        "status" => vec![
            key(
                SortField::Status,
                SortDirection::Asc,
                Some(DEFAULT_STATUS_RANK.to_vec()),
            ),
            key(SortField::Id, SortDirection::Asc, None),
        ],
        "active" => vec![
            key(
                SortField::Status,
                SortDirection::Asc,
                Some(vec![
                    TaskStatus::InProgress,
                    TaskStatus::Pending,
                    TaskStatus::Completed,
                ]),
            ),
            key(SortField::Id, SortDirection::Asc, None),
        ],
        "recent" => vec![
            key(SortField::UpdatedAt, SortDirection::Desc, None),
            key(SortField::Id, SortDirection::Desc, None),
        ],
        "oldest" => vec![
            key(SortField::UpdatedAt, SortDirection::Asc, None),
            key(SortField::Id, SortDirection::Asc, None),
        ],
        _ => return None,
    })
}

/// 从 JSON 值解析比较字段名（snake_case，大小写不敏感）；非法返回 `None`。
fn parse_field(v: &Value) -> Option<SortField> {
    v.as_str()?.trim().parse().ok()
}

/// 解析升降序：缺失/`null` 默认 `Asc`，非法值返回 `None`。
fn parse_direction(v: Option<&Value>) -> Option<SortDirection> {
    match v {
        None | Some(Value::Null) => Some(SortDirection::Asc),
        Some(Value::String(s)) => s.trim().parse().ok(),
        _ => None,
    }
}

/// 解析 status 排序的自定义状态顺序：缺失/`null` 为 `Some(None)`（用默认序），元素非法返回 `None`。
fn parse_rank(v: Option<&Value>) -> Option<Option<Vec<TaskStatus>>> {
    match v {
        None | Some(Value::Null) => Some(None),
        Some(Value::Array(items)) => {
            let mut rank = Vec::with_capacity(items.len());
            for item in items {
                rank.push(TaskStatus::parse(item.as_str()?)?);
            }
            Some(Some(rank))
        }
        _ => None,
    }
}

/// 解析一个 `{field, direction, rank}` 比较键对象；缺字段或类型非法返回 `None`。
fn parse_key(v: &Value) -> Option<SortKey> {
    let obj = v.as_object()?;
    let field = parse_field(obj.get("field")?)?;
    let direction = parse_direction(obj.get("direction"))?;
    let rank = parse_rank(obj.get("rank"))?;
    Some(SortKey {
        field,
        direction,
        rank,
    })
}

/// 把配置值解析为 spec；无法识别一律回退 `id`（不抛错、不拒绝）。
fn to_spec(order: Option<&Value>) -> SortSpec {
    match order {
        Some(Value::String(name)) => preset(name).unwrap_or_else(|| preset("id").unwrap()),
        Some(Value::Array(items)) if !items.is_empty() => {
            let mut spec = Vec::with_capacity(items.len());
            for item in items {
                match parse_key(item) {
                    Some(k) => spec.push(k),
                    None => return preset("id").unwrap(),
                }
            }
            spec
        }
        _ => preset("id").unwrap(),
    }
}

/// 状态在 rank 中的位置；未列出者统一排在末尾并相互并列。
fn status_index(status: TaskStatus, rank: &[TaskStatus]) -> usize {
    rank.iter().position(|s| *s == status).unwrap_or(rank.len())
}

/// 按单个比较键比较两个任务（不含升降方向）：status 用 rank 序，id 用数字序，updated_at 直接比时间戳。
fn compare_key(a: &Task, b: &Task, key: &SortKey) -> Ordering {
    match key.field {
        SortField::Status => {
            let fallback = DEFAULT_STATUS_RANK.to_vec();
            let rank = key.rank.as_deref().unwrap_or(&fallback);
            status_index(a.status, rank).cmp(&status_index(b.status, rank))
        }
        SortField::Id => {
            let (x, y) = (numeric_id(&a.id), numeric_id(&b.id));
            x.partial_cmp(&y).unwrap_or(Ordering::Equal)
        }
        SortField::UpdatedAt => a.updated_at.cmp(&b.updated_at),
    }
}

/// 返回排序后的副本，不改动入参。
pub fn sort_tasks(tasks: &[Task], order: Option<&Value>) -> Vec<Task> {
    let spec = to_spec(order);
    let mut out = tasks.to_vec();

    out.sort_by(|a, b| {
        for key in &spec {
            let delta = compare_key(a, b, key);
            if delta != Ordering::Equal {
                return match key.direction {
                    SortDirection::Asc => delta,
                    SortDirection::Desc => delta.reverse(),
                };
            }
        }
        Ordering::Equal
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn task(id: &str, status: TaskStatus, updated_at: u64) -> Task {
        Task {
            id: id.to_string(),
            subject: format!("t{id}"),
            description: String::new(),
            status,
            active_form: None,
            owner: None,
            metadata: Default::default(),
            blocks: Vec::new(),
            blocked_by: Vec::new(),
            created_at: 0,
            updated_at,
        }
    }

    fn ids(tasks: &[Task]) -> Vec<String> {
        tasks.iter().map(|t| t.id.clone()).collect()
    }

    #[test]
    fn id_is_default_and_numeric() {
        let tasks = vec![
            task("10", TaskStatus::Pending, 0),
            task("2", TaskStatus::Pending, 0),
            task("1", TaskStatus::Pending, 0),
        ];
        assert_eq!(ids(&sort_tasks(&tasks, None)), ["1", "2", "10"]);
        assert_eq!(
            ids(&sort_tasks(&tasks, Some(&json!("bogus")))),
            ["1", "2", "10"],
            "未知预设回退 id"
        );
    }

    #[test]
    fn status_preset_is_completed_first() {
        let tasks = vec![
            task("1", TaskStatus::Pending, 0),
            task("2", TaskStatus::Completed, 0),
            task("3", TaskStatus::InProgress, 0),
        ];
        assert_eq!(
            ids(&sort_tasks(&tasks, Some(&json!("status")))),
            ["2", "3", "1"]
        );
    }

    #[test]
    fn active_preset_is_in_progress_first() {
        let tasks = vec![
            task("1", TaskStatus::Pending, 0),
            task("2", TaskStatus::Completed, 0),
            task("3", TaskStatus::InProgress, 0),
        ];
        assert_eq!(
            ids(&sort_tasks(&tasks, Some(&json!("active")))),
            ["3", "1", "2"]
        );
    }

    #[test]
    fn recent_and_oldest_use_updated_at() {
        let tasks = vec![
            task("1", TaskStatus::Pending, 10),
            task("2", TaskStatus::Pending, 30),
            task("3", TaskStatus::Pending, 20),
        ];
        assert_eq!(
            ids(&sort_tasks(&tasks, Some(&json!("recent")))),
            ["2", "3", "1"]
        );
        assert_eq!(
            ids(&sort_tasks(&tasks, Some(&json!("oldest")))),
            ["1", "3", "2"]
        );
    }

    #[test]
    fn custom_spec_and_direction() {
        let tasks = vec![
            task("1", TaskStatus::Pending, 0),
            task("2", TaskStatus::Completed, 0),
            task("3", TaskStatus::InProgress, 0),
        ];
        let spec = json!([
            { "field": "status", "rank": ["in_progress", "pending", "completed"] },
            { "field": "id", "direction": "desc" }
        ]);
        assert_eq!(ids(&sort_tasks(&tasks, Some(&spec))), ["3", "1", "2"]);
    }

    #[test]
    fn malformed_spec_falls_back_to_id() {
        let tasks = vec![
            task("2", TaskStatus::Pending, 0),
            task("1", TaskStatus::Pending, 0),
        ];
        assert_eq!(
            ids(&sort_tasks(&tasks, Some(&json!([{ "field": "nope" }])))),
            ["1", "2"]
        );
    }
}
