//! 任务存储
//!
//! 两种模式：无路径 = 纯内存（零磁盘 IO）；有路径 = 文件支撑，
//! 写操作在跨进程文件锁（[`crate::utils::file_lock`]）下重新加载 → 变更 → 原子写。
//!
//! 健壮性契约：
//! - 每条记录在加载边界归一化，手写文件里的错类型不会让整个 store 崩；
//! - 信封（`tasks` 数组、`nextId`）也要校验：截断写/坏合并/手改会留下「能解析但不可用」的文件，
//!   过去会让 store 半加载或产出 `NaN`/碰撞 id；现在不可用就保持原状；
//! - 写是 tmp + rename 的原子替换。

use crate::{
    extensions::tasks::{sort::sort_tasks, types::*},
    utils::{file_lock::FileLock, time::now_ms},
};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

/// 文件支撑的任务存储。无路径时为纯内存。
#[derive(Debug, Default)]
pub struct TaskStore {
    /// 落盘路径；`None` 表示纯内存模式。
    file_path: Option<PathBuf>,
    /// 下一个要分配的任务 id。
    next_id: u64,
    /// 任务表，按 id 索引。
    tasks: BTreeMap<String, Task>,
}

impl TaskStore {
    /// 构造存储。`path = None` 为纯内存。
    ///
    /// 目录**惰性**创建（首次写入时取锁/save 各自 mkdir）
    pub fn new(path: Option<PathBuf>) -> Self {
        let mut store = TaskStore {
            next_id: 1,
            ..Default::default()
        };

        if let Some(path) = path {
            store.file_path = Some(path);
            store.load();
        }
        store
    }

    /// 任务数（测试与诊断用）。
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.tasks.len()
    }

    /// 列出全部任务（可选排序）。文件支撑时先重读磁盘，以看到其它会话的写入。
    pub fn list(&mut self, order: Option<&Value>) -> Vec<Task> {
        self.load();
        let all: Vec<Task> = self.tasks.values().cloned().collect();
        sort_tasks(&all, order)
    }

    /// 先重读磁盘（看到其它会话的写入），再按 id 返回任务的副本；不存在时为 None。
    pub fn get(&mut self, id: &str) -> Option<Task> {
        self.load();
        self.tasks.get(id).cloned()
    }

    /// 列出内存中的任务，**不重读磁盘**。供 `dock_lines` 每帧调用（读盘太贵）；
    /// 跨会话可见性由回合/工具时机上的 `list()` 刷新负责。
    pub fn list_cached(&self, order: Option<&Value>) -> Vec<Task> {
        let all: Vec<Task> = self.tasks.values().cloned().collect();
        sort_tasks(&all, order)
    }

    /// 从内存取一条任务，不重读磁盘（供 widget 的阻塞者查询）。
    pub fn cached_get(&self, id: &str) -> Option<Task> {
        self.tasks.get(id).cloned()
    }

    /// 取全量快照。供 fork 继承任务用。
    pub fn snapshot(&mut self) -> TaskStoreData {
        self.load();
        TaskStoreData {
            next_id: self.next_id,
            tasks: self.tasks.values().cloned().collect(),
        }
    }

    /// 新建一条 Pending 任务，id 取自增序号；锁内落盘，写入失败时返回 Err。
    pub fn create(
        &mut self,
        subject: String,
        description: String,
        active_form: Option<String>,
        metadata: Option<Metadata>,
    ) -> Result<Task, String> {
        self.with_lock(|store| {
            let now = now_ms();
            let task = Task {
                id: store.next_id.to_string(),
                subject,
                description,
                status: TaskStatus::Pending,
                active_form,
                owner: None,
                metadata: metadata.unwrap_or_default(),
                blocks: Vec::new(),
                blocked_by: Vec::new(),
                created_at: now,
                updated_at: now,
            };
            store.next_id += 1;
            store.tasks.insert(task.id.clone(), task.clone());
            Ok(task)
        })
    }

    /// 按 id 应用部分字段更新（含 status/subject/metadata 与双向依赖边），
    /// `status = Deleted` 时删除任务并清理其它任务对它的依赖引用。
    /// 任务不存在时返回全空的 `UpdateOutcome`（不报错）；落盘失败或非法变更返回 Err。
    /// `UpdateOutcome.warnings` 记录自环、互相阻塞、目标不存在等依赖告警。
    pub fn update(&mut self, id: &str, fields: TaskUpdateFields) -> Result<UpdateOutcome, String> {
        self.with_lock(|store| {
            let Some(mut task) = store.tasks.get(id).cloned() else {
                return Ok(UpdateOutcome::default());
            };

            let mut changed: Vec<String> = Vec::new();
            let mut warnings: Vec<String> = Vec::new();

            // 删除
            if fields.status == Some(TaskStatusOrDeleted::Deleted) {
                store.tasks.remove(id);
                for t in store.tasks.values_mut() {
                    t.blocks.retain(|b| b != id);
                    t.blocked_by.retain(|b| b != id);
                }
                return Ok(UpdateOutcome {
                    task: None,
                    changed_fields: vec!["deleted".to_string()],
                    warnings: Vec::new(),
                });
            }

            if let Some(TaskStatusOrDeleted::Status(s)) = fields.status {
                task.status = s;
                changed.push("status".to_string());
            }
            if let Some(v) = fields.subject {
                task.subject = v;
                changed.push("subject".to_string());
            }
            if let Some(v) = fields.description {
                task.description = v;
                changed.push("description".to_string());
            }
            if let Some(v) = fields.active_form {
                task.active_form = Some(v);
                changed.push("activeForm".to_string());
            }
            if let Some(v) = fields.owner {
                task.owner = Some(v);
                changed.push("owner".to_string());
            }
            if let Some(meta) = fields.metadata {
                // 浅合并：null 删键。
                for (k, v) in meta {
                    if v.is_null() {
                        task.metadata.remove(&k);
                    } else {
                        task.metadata.insert(k, v);
                    }
                }
                changed.push("metadata".to_string());
            }

            // 双向依赖边
            if let Some(targets) = fields.add_blocks.filter(|v| !v.is_empty()) {
                for target_id in targets {
                    if !task.blocks.contains(&target_id) {
                        task.blocks.push(target_id.clone());
                    }

                    if target_id == id {
                        if !contains_id(&task.blocked_by, id) {
                            task.blocked_by.push(id.to_string());
                        }
                        warnings.push(format!("#{id} blocks itself"));
                    } else {
                        match store.tasks.get_mut(&target_id) {
                            Some(target) => {
                                if !contains_id(&target.blocked_by, id) {
                                    target.blocked_by.push(id.to_string());
                                    target.updated_at = now_ms();
                                }
                                if contains_id(&target.blocks, id) {
                                    warnings.push(format!(
                                        "cycle: #{id} and #{target_id} block each other"
                                    ));
                                }
                            }
                            None => warnings.push(format!("#{target_id} does not exist")),
                        }
                    }
                }
                changed.push("blocks".to_string());
            }

            if let Some(targets) = fields.add_blocked_by.filter(|v| !v.is_empty()) {
                for target_id in targets {
                    if !task.blocked_by.contains(&target_id) {
                        task.blocked_by.push(target_id.clone());
                    }

                    if target_id == id {
                        if !contains_id(&task.blocks, id) {
                            task.blocks.push(id.to_string());
                        }
                        warnings.push(format!("#{id} blocks itself"));
                    } else {
                        match store.tasks.get_mut(&target_id) {
                            Some(target) => {
                                if !contains_id(&target.blocks, id) {
                                    target.blocks.push(id.to_string());
                                    target.updated_at = now_ms();
                                }
                            }
                            None => warnings.push(format!("#{target_id} does not exist")),
                        }

                        if task.blocks.contains(&target_id) {
                            warnings
                                .push(format!("cycle: #{id} and #{target_id} block each other"));
                        }
                    }
                }
                changed.push("blockedBy".to_string());
            }

            task.updated_at = now_ms();
            store.tasks.insert(id.to_string(), task.clone());
            Ok(UpdateOutcome {
                task: Some(task),
                changed_fields: changed,
                warnings,
            })
        })
    }

    /// 按 id 删除。返回是否删掉了。
    pub fn delete(&mut self, id: &str) -> Result<bool, String> {
        self.with_lock(|store| {
            if store.tasks.remove(id).is_none() {
                return Ok(false);
            }

            for t in store.tasks.values_mut() {
                t.blocks.retain(|b| b != id);
                t.blocked_by.retain(|b| b != id);
            }
            Ok(true)
        })
    }

    /// 删除所有任务，返回删除数量。
    pub fn clear_all(&mut self) -> Result<usize, String> {
        self.with_lock(|store| {
            let n = store.tasks.len();
            store.tasks.clear();
            Ok(n)
        })
    }

    /// 删除所有已完成任务，返回删除数量。
    pub fn clear_completed(&mut self) -> Result<usize, String> {
        self.with_lock(|store| {
            let before = store.tasks.len();
            store.tasks.retain(|_, t| t.status != TaskStatus::Completed);
            let removed = before - store.tasks.len();

            if removed > 0 {
                let valid: BTreeSet<String> = store.tasks.keys().cloned().collect();
                for t in store.tasks.values_mut() {
                    t.blocks.retain(|b| valid.contains(b));
                    t.blocked_by.retain(|b| valid.contains(b));
                }
            }
            Ok(removed)
        })
    }

    /// 从快照灌入一个空 store。store 已有任务时是 no-op（重复指向已 seed 的 fork 文件不重复）。
    pub fn seed(&mut self, data: TaskStoreData) -> Result<(), String> {
        if !self.tasks.is_empty() {
            return Ok(());
        }

        self.with_lock(|store| {
            store.next_id = data.next_id;
            store.tasks.clear();

            for t in data.tasks {
                store.tasks.insert(t.id.clone(), t);
            }
            Ok(())
        })
    }

    /// 若文件支撑且已空，删除支撑文件。返回是否删了。
    pub fn delete_file_if_empty(&self) -> bool {
        if self.tasks.is_empty()
            && let Some(path) = &self.file_path
        {
            _ = fs::remove_file(path);
            return true;
        }

        false
    }

    /// 在跨进程文件锁内做一次「重读 → 变更 → 原子写」。
    fn with_lock<T>(
        &mut self,
        f: impl FnOnce(&mut TaskStore) -> Result<T, String>,
    ) -> Result<T, String> {
        let Some(path) = self.file_path.clone() else {
            return f(self);
        };

        // 锁覆盖整段读-改-写：否则两个会话各自基于旧快照回写，后写者会丢前写者的变更。
        let _lock = FileLock::acquire(&path).map_err(|e| e.to_string())?;
        self.load(); // 变更前重读最新状态
        let result = f(self);
        let save_result = self.save();
        save_result?;
        result
    }

    /// 从磁盘重读任务集与自增序号；未配置路径或读取/解析失败时保持内存现状。
    fn load(&mut self) {
        if let Some((next_id, tasks)) = load_from(self.file_path.as_deref()) {
            self.next_id = next_id;
            self.tasks = tasks;
        }
    }

    /// 将当前任务集原子写入文件（先写 .tmp 再 rename）；未配置路径时直接成功。
    /// 序列化或 IO 失败时返回 Err。
    fn save(&self) -> Result<(), String> {
        let Some(path) = self.file_path.as_deref() else {
            return Ok(());
        };

        let data = TaskStoreData {
            next_id: self.next_id,
            tasks: self.tasks.values().cloned().collect(),
        };

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }

        let tmp = path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(&data).map_err(|e| format!("ser: {e}"))?;
        fs::write(&tmp, text).map_err(|e| format!("write {}: {e}", tmp.display()))?;
        fs::rename(&tmp, path)
            .map_err(|e| format!("rename {} -> {}: {e}", tmp.display(), path.display()))?;
        Ok(())
    }
}

/// 依赖边列表是否包含某 id（Vec<String> 与 &str 比较）。
fn contains_id(list: &[String], id: &str) -> bool {
    list.iter().any(|x| x == id)
}

/// 读取并校验一个 store 文件。返回 `None` = 不可用（缺失/坏 JSON/坏信封），此时调用方应保持原状态。
fn load_from(path: Option<&Path>) -> Option<(u64, BTreeMap<String, Task>)> {
    let path = path?;
    let text = fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    let obj = value.as_object()?;
    let tasks_value = obj.get("tasks")?.as_array()?;

    let mut tasks = BTreeMap::new();
    let mut max_id = 0u64;
    for t in tasks_value {
        if let Some(task) = normalize_task(t) {
            if let Ok(numeric) = task.id.parse::<u64>()
                && numeric > max_id
            {
                max_id = numeric;
            }
            tasks.insert(task.id.clone(), task);
        }
    }

    // 每个未来的 task id 都来自这个计数器，因此它必须越过已在用的 id——
    // 无论文件是省略了它还是记了个陈旧值。
    let next_id = match obj.get("nextId").and_then(Value::as_u64) {
        Some(n) if n > max_id => n,
        _ => max_id + 1,
    };
    Some((next_id, tasks))
}

/// 归一化一条记录；缺字段补默认、错类型回退，`id` 不可用则丢弃。
fn normalize_task(v: &Value) -> Option<Task> {
    let obj = v.as_object()?;
    let id = obj.get("id")?.as_str()?.to_string();

    let string_array = |key: &str| -> Vec<String> {
        obj.get(key)
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };

    let now = now_ms();
    Some(Task {
        id,
        subject: obj
            .get("subject")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        description: obj
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        status: obj
            .get("status")
            .and_then(Value::as_str)
            .and_then(TaskStatus::parse)
            .unwrap_or(TaskStatus::Pending),
        active_form: obj
            .get("activeForm")
            .and_then(Value::as_str)
            .map(str::to_string),
        owner: obj.get("owner").and_then(Value::as_str).map(str::to_string),
        metadata: obj
            .get("metadata")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default(),
        blocks: string_array("blocks"),
        blocked_by: string_array("blockedBy"),
        created_at: obj.get("createdAt").and_then(Value::as_u64).unwrap_or(now),
        updated_at: obj.get("updatedAt").and_then(Value::as_u64).unwrap_or(now),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn mem() -> TaskStore {
        TaskStore::new(None)
    }

    #[test]
    fn crud_and_dependencies() {
        let mut store = mem();
        let a = store.create("A".into(), "da".into(), None, None).unwrap();
        let b = store.create("B".into(), "db".into(), None, None).unwrap();
        assert_eq!(a.id, "1");
        assert_eq!(b.id, "2");

        let out = store
            .update(
                "1",
                TaskUpdateFields {
                    add_blocks: Some(vec!["2".into()]),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(out.changed_fields, vec!["blocks"]);
        assert_eq!(store.get("1").unwrap().blocks, vec!["2"]);
        assert_eq!(store.get("2").unwrap().blocked_by, vec!["1"]);

        // 删除清理边
        assert!(store.delete("1").unwrap());
        assert!(store.get("2").unwrap().blocked_by.is_empty());
    }

    #[test]
    fn warnings_for_self_cycle_and_dangling() {
        let mut store = mem();
        store.create("A".into(), "d".into(), None, None).unwrap();
        let out = store
            .update(
                "1",
                TaskUpdateFields {
                    add_blocks: Some(vec!["1".into(), "9".into()]),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(out.warnings.iter().any(|w| w.contains("blocks itself")));
        assert!(out.warnings.iter().any(|w| w.contains("does not exist")));
    }

    #[test]
    fn metadata_null_deletes_key() {
        let mut store = mem();
        let mut meta = Metadata::new();
        meta.insert("agentType".into(), json!("Explore"));
        store
            .create("A".into(), "d".into(), None, Some(meta))
            .unwrap();

        let mut patch = Metadata::new();
        patch.insert("agentType".into(), Value::Null);
        patch.insert("x".into(), json!(1));
        store
            .update(
                "1",
                TaskUpdateFields {
                    metadata: Some(patch),
                    ..Default::default()
                },
            )
            .unwrap();
        let t = store.get("1").unwrap();
        assert!(!t.metadata.contains_key("agentType"));
        assert_eq!(t.metadata.get("x"), Some(&json!(1)));
    }

    #[test]
    fn file_roundtrip_and_envelope_validation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tasks.json");
        {
            let mut store = TaskStore::new(Some(path.clone()));
            store.create("A".into(), "d".into(), None, None).unwrap();
            store.create("B".into(), "d".into(), None, None).unwrap();
        }
        let mut store = TaskStore::new(Some(path.clone()));
        assert_eq!(store.len(), 2);
        assert_eq!(store.snapshot().next_id, 3);

        // 坏信封：tasks 不是数组 → 保持空状态、不崩
        fs::write(&path, r#"{"nextId": 5}"#).unwrap();
        let store = TaskStore::new(Some(path.clone()));
        assert_eq!(store.len(), 0);

        // 坏记录被跳过；nextId 陈旧时抬起越过 maxId
        fs::write(
            &path,
            r#"{"nextId": 1, "tasks": [{"id": "7", "subject": "s"}, 42]}"#,
        )
        .unwrap();
        let mut store = TaskStore::new(Some(path.clone()));
        assert_eq!(store.len(), 1);
        assert_eq!(store.snapshot().next_id, 8);
    }

    #[test]
    fn seed_into_empty_and_noop_when_nonempty() {
        let mut store = mem();
        store.create("A".into(), "d".into(), None, None).unwrap();
        let data = TaskStoreData {
            next_id: 10,
            tasks: vec![Task {
                id: "5".into(),
                subject: "seeded".into(),
                description: String::new(),
                status: TaskStatus::Pending,
                active_form: None,
                owner: None,
                metadata: Metadata::new(),
                blocks: vec![],
                blocked_by: vec![],
                created_at: 1,
                updated_at: 1,
            }],
        };
        store.seed(data.clone()).unwrap();
        assert_eq!(store.len(), 1, "非空 store 不种子");

        let mut empty = mem();
        empty.seed(data).unwrap();
        assert_eq!(empty.get("5").unwrap().subject, "seeded");
        assert_eq!(empty.snapshot().next_id, 10);
    }

    #[test]
    fn clear_completed_prunes_edges() {
        let mut store = mem();
        store.create("A".into(), "d".into(), None, None).unwrap();
        store.create("B".into(), "d".into(), None, None).unwrap();
        store
            .update(
                "1",
                TaskUpdateFields {
                    add_blocks: Some(vec!["2".into()]),
                    ..Default::default()
                },
            )
            .unwrap();
        store
            .update(
                "1",
                TaskUpdateFields {
                    status: Some(TaskStatusOrDeleted::Status(TaskStatus::Completed)),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(store.clear_completed().unwrap(), 1);
        assert!(store.get("2").unwrap().blocks.is_empty());
    }

    #[test]
    fn concurrent_updates_are_locked() {
        // 同进程多线程并发写同一个文件——锁文件保证不丢更新。
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tasks.json");
        let path_a = path.clone();
        std::thread::scope(|s| {
            for i in 0..8 {
                let p = path_a.clone();
                s.spawn(move || {
                    let mut store = TaskStore::new(Some(p));
                    store
                        .create(format!("t{i}"), "d".into(), None, None)
                        .unwrap();
                });
            }
        });
        let store = TaskStore::new(Some(path));
        assert_eq!(store.len(), 8);
    }
}
