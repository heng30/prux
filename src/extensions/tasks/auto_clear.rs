//! 回合制自动清理已完成任务
//!
//! 两种模式：
//! - `on_task_complete`：每个完成的任务各自有一个 `delay` 倒计时，逐个删除；
//! - `on_list_complete`：所有任务都完成时开始倒计时，整批删除。
//!
//! 两个倒计时都以**回合**计量、只在 `turn_start` 走字，因此 agent 一停就冻住。
//! 对话继续时没问题，但「最后一次完成之后立刻结束 run」是常态，会让已完成列表带着
//! 冻住的倒计时留在屏幕上，下一批工作就会加到它上面。
//!
//! `start_new_batch()` 覆盖这种情况。它必须区分「新批次」与「同一批次还在继续」，
//! 而 store 区分不了：agent 刚清完的列表上再加一个任务，两种情况看起来一样。
//! run 边界才是分界——在完成该列表的 run 内新增的工作属于它，run 结束之后新增的不属于
//! ——因此清空由 `on_run_ended()` 布防。

use super::{config::AutoClearMode, store::TaskStore, types::TaskStatus};
use std::collections::BTreeMap;

/// 回合制自动清理管理器。
#[derive(Debug, Default)]
pub struct AutoClearManager {
    /// 每个任务的完成回合（`on_task_complete` 模式）。
    completed_at_turn: BTreeMap<String, u64>,
    /// 全部任务变为完成的回合（`on_list_complete` 模式）。
    all_completed_at_turn: Option<u64>,
    /// 自当前列表最后一次新增以来，是否已有 run 结束。
    run_ended: bool,
    /// 完成后逗留多少个回合再清理。
    delay: u64,
}

impl AutoClearManager {
    /// 新建管理器。`delay`：任务完成后还需经过的回合数才清理。
    pub fn new(delay: u64) -> Self {
        AutoClearManager {
            delay,
            ..Default::default()
        }
    }

    /// 记录一次任务完成。应在 cascade 逻辑**之后**调用。
    pub fn track_completion(
        &mut self,
        store: &mut TaskStore,
        task_id: &str,
        turn: u64,
        mode: AutoClearMode,
    ) {
        match mode {
            AutoClearMode::Never => {}
            AutoClearMode::OnTaskComplete => {
                self.completed_at_turn.insert(task_id.to_string(), turn);
            }
            AutoClearMode::OnListComplete => self.check_all_completed(store, turn),
        }
    }

    /// `on_list_complete` 模式下刷新「全部完成」时刻：列表非空且所有任务都是
    /// Completed 时记录首个满足条件的回合（已记录则不覆盖），否则清空该记录。
    fn check_all_completed(&mut self, store: &mut TaskStore, turn: u64) {
        let tasks = store.list(None);
        if !tasks.is_empty() && tasks.iter().all(|t| t.status == TaskStatus::Completed) {
            if self.all_completed_at_turn.is_none() {
                self.all_completed_at_turn = Some(turn);
            }
        } else {
            self.all_completed_at_turn = None;
        }
    }

    /// 重置整批倒计时（新任务创建、任务转回未完成时）。
    pub fn reset_batch_countdown(&mut self) {
        self.all_completed_at_turn = None;
    }

    /// 不会再有自动重试/压缩/排队的续跑，因此当前列表就是这个 run 的最终态。
    /// 对带入 resume/fork 会话的列表同样成立：产出它的 run 随上一个会话结束了。
    pub fn on_run_ended(&mut self) {
        self.run_ended = true;
    }

    /// 一个任务即将被创建。若自本列表最后一次新增以来已有 run 结束、且列表上已无事可做，
    /// 该列表就属于上一批次——清掉它，让新任务从干净列表开始，而不是追加到用户已看过完成的那些行上。
    ///
    /// 否则不动：列表里还有未完成的工作，或 agent 仍在同一 run 内继续搭建它
    /// （create→complete→create），那样会在下一步一加进来就丢掉每一步。
    pub fn start_new_batch(&mut self, store: &mut TaskStore, mode: AutoClearMode) {
        self.all_completed_at_turn = None;
        let after_finished_run = self.run_ended;
        self.run_ended = false;

        if !after_finished_run || mode == AutoClearMode::Never {
            return;
        }

        let tasks = store.list(None);
        if !tasks.is_empty() && tasks.iter().all(|t| t.status == TaskStatus::Completed) {
            _ = store.clear_completed();
            self.completed_at_turn.clear();
        }
    }

    /// 重置全部跟踪状态（新会话）。
    pub fn reset(&mut self) {
        self.completed_at_turn.clear();
        self.all_completed_at_turn = None;
        self.run_ended = false;
    }

    /// 每个 `turn_start` 调用。删除逗留期已过的任务。返回是否有清理发生。
    pub fn on_turn_start(&mut self, store: &mut TaskStore, turn: u64, mode: AutoClearMode) -> bool {
        let mut cleared = false;
        match mode {
            AutoClearMode::Never => {}
            AutoClearMode::OnTaskComplete => {
                let due: Vec<String> = self
                    .completed_at_turn
                    .iter()
                    .filter_map(|(id, t)| {
                        let task = store.get(id);
                        match task {
                            Some(task) if task.status == TaskStatus::Completed => {
                                (turn.saturating_sub(*t) >= self.delay).then(|| id.clone())
                            }
                            _ => Some(id.clone()), // 已删除/回退 → 清理陈旧跟踪项
                        }
                    })
                    .collect();

                for id in due {
                    if let Some(task) = store.get(&id)
                        && task.status == TaskStatus::Completed
                    {
                        _ = store.delete(&id);
                        cleared = true;
                    }

                    self.completed_at_turn.remove(&id);
                }
            }
            AutoClearMode::OnListComplete => {
                if let Some(at) = self.all_completed_at_turn
                    && turn.saturating_sub(at) >= self.delay
                {
                    _ = store.clear_completed();
                    self.all_completed_at_turn = None;
                    cleared = true;
                }
            }
        }
        cleared
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with(n: usize) -> (TaskStore, Vec<String>) {
        let mut s = TaskStore::new(None);
        let mut ids = Vec::new();
        for i in 0..n {
            ids.push(
                s.create(format!("t{i}"), "d".into(), None, None)
                    .unwrap()
                    .id,
            );
        }
        (s, ids)
    }

    fn complete(store: &mut TaskStore, id: &str) {
        store
            .update(
                id,
                super::super::types::TaskUpdateFields {
                    status: Some(super::super::types::TaskStatusOrDeleted::Status(
                        TaskStatus::Completed,
                    )),
                    ..Default::default()
                },
            )
            .unwrap();
    }

    #[test]
    fn on_list_complete_clears_after_delay() {
        let (mut store, ids) = store_with(2);
        let mut m = AutoClearManager::new(4);
        complete(&mut store, &ids[0]);
        m.track_completion(&mut store, &ids[0], 0, AutoClearMode::OnListComplete);
        assert!(!m.on_turn_start(&mut store, 2, AutoClearMode::OnListComplete));
        complete(&mut store, &ids[1]);
        m.track_completion(&mut store, &ids[1], 2, AutoClearMode::OnListComplete);
        assert!(m.on_turn_start(&mut store, 6, AutoClearMode::OnListComplete));
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn on_task_complete_clears_individually() {
        let (mut store, ids) = store_with(2);
        let mut m = AutoClearManager::new(4);
        complete(&mut store, &ids[0]);
        m.track_completion(&mut store, &ids[0], 0, AutoClearMode::OnTaskComplete);
        assert!(m.on_turn_start(&mut store, 4, AutoClearMode::OnTaskComplete));
        assert_eq!(store.len(), 1, "只清了一个");
        assert!(store.get(&ids[1]).is_some());
    }

    #[test]
    fn start_new_batch_retires_finished_list_after_run() {
        let (mut store, ids) = store_with(1);
        let mut m = AutoClearManager::new(4);
        complete(&mut store, &ids[0]);
        m.on_run_ended();
        m.start_new_batch(&mut store, AutoClearMode::OnListComplete);
        assert_eq!(store.len(), 0, "run 已结束的完成列表被清空");
    }

    #[test]
    fn start_new_batch_keeps_list_being_built() {
        let (mut store, ids) = store_with(2);
        let mut m = AutoClearManager::new(4);
        complete(&mut store, &ids[0]);
        // 未 on_run_ended：同一 run 内继续搭建 → 保留
        m.start_new_batch(&mut store, AutoClearMode::OnListComplete);
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn never_mode_never_clears() {
        let (mut store, ids) = store_with(1);
        let mut m = AutoClearManager::new(0);
        complete(&mut store, &ids[0]);
        m.track_completion(&mut store, &ids[0], 0, AutoClearMode::Never);
        m.on_run_ended();
        m.start_new_batch(&mut store, AutoClearMode::Never);
        assert_eq!(store.len(), 1);
        assert!(!m.on_turn_start(&mut store, 99, AutoClearMode::Never));
        assert_eq!(store.len(), 1);
    }
}
