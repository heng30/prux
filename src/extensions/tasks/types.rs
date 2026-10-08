//! 任务模型
//!
//! 字段名与状态词刻意镜像 Claude Code 的任务工具约定：
//! `pending` → `in_progress` → `completed`，`deleted` 作为 `TaskUpdate` 的删除指令。

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use strum_macros::{EnumString, IntoStaticStr};

/// 任务元数据：任意键值（`agentType` / `agentId` / `result` / `lastError` 等）。
pub type Metadata = Map<String, Value>;

/// 任务状态。
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    EnumString,
    IntoStaticStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case", ascii_case_insensitive)]
pub enum TaskStatus {
    /// 待办，尚未开始。
    Pending = 0,
    /// 进行中。
    InProgress = 1,
    /// 已完成。
    Completed = 2,
}

impl TaskStatus {
    /// 状态词（`pending` / `in_progress` / `completed`），用于序列化与展示。
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// 解析状态词；未知值返回 `None`（hand-edited 文件容错入口）。
    pub fn parse(s: &str) -> Option<Self> {
        s.trim().parse().ok()
    }
}

/// 一条任务。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Task {
    /// 任务 id（数字字符串，由 store 的 `nextId` 递增分配）。
    pub id: String,
    /// 简短标题（祈使句形式）。
    pub subject: String,
    /// 详细描述与验收标准。
    pub description: String,
    /// 当前状态。
    pub status: TaskStatus,
    /// 进行中时 spinner 展示的现在进行时文案；缺省时回退 `subject`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_form: Option<String>,
    /// 任务归属者（agent 名）；未分配为空。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// 任意附加元数据（`agentType` / `agentId` / `result` / `lastError` 等）。
    #[serde(default)]
    pub metadata: Metadata,
    /// 本任务阻塞的任务 id（完成后它们才能开始）。
    #[serde(default)]
    pub blocks: Vec<String>,
    /// 阻塞本任务的任务 id（它们完成后本任务才能开始）。
    #[serde(default)]
    pub blocked_by: Vec<String>,
    /// 创建时间（毫秒时间戳）。
    pub created_at: u64,
    /// 最后更新时间（毫秒时间戳）。
    pub updated_at: u64,
}

/// 磁盘上的存储信封。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskStoreData {
    /// 下一个要分配的 id 计数器。
    pub next_id: u64,
    /// 全部任务记录。
    pub tasks: Vec<Task>,
}

/// `TaskUpdate` 的字段集合（`status` 额外承载 `deleted` 指令）。
#[derive(Debug, Clone, Default)]
pub struct TaskUpdateFields {
    /// 新状态；`Deleted` 表示删除该任务。
    pub status: Option<TaskStatusOrDeleted>,
    /// 新标题。
    pub subject: Option<String>,
    /// 新描述。
    pub description: Option<String>,
    /// 新的进行时文案。
    pub active_form: Option<String>,
    /// 新归属者。
    pub owner: Option<String>,
    /// 要浅合并的元数据（值为 null 时删除对应键）。
    pub metadata: Option<Metadata>,
    /// 追加的本任务阻塞目标 id。
    pub add_blocks: Option<Vec<String>>,
    /// 追加的阻塞本任务的目标 id。
    pub add_blocked_by: Option<Vec<String>>,
}

/// `TaskUpdate` 的 `status` 取值：三态 + `deleted`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatusOrDeleted {
    /// 设置为某个常规状态。
    Status(TaskStatus),
    /// 删除该任务。
    Deleted,
}

impl TaskStatusOrDeleted {
    /// 解析 `TaskUpdate.status` 取值：`deleted` 得到 Deleted，其余交给
    /// [`TaskStatus::parse`]；未知词返回 None。
    pub fn parse(s: &str) -> Option<Self> {
        if s == "deleted" {
            return Some(TaskStatusOrDeleted::Deleted);
        }
        TaskStatus::parse(s).map(TaskStatusOrDeleted::Status)
    }
}

/// 一次更新的结果：变更字段与依赖告警（供工具响应拼装）。
#[derive(Debug, Clone, Default)]
pub struct UpdateOutcome {
    /// 更新后的任务；删除时为 `None`。
    pub task: Option<Task>,
    /// 本次实际变更的字段名列表。
    pub changed_fields: Vec<String>,
    /// 依赖边产生的告警（自环、互相阻塞、目标不存在等）。
    pub warnings: Vec<String>,
}
