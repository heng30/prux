//! 整列表完成后的墙钟自动隐藏
//!
//! 与 [`super::auto_clear`] 的回合制清理相互独立：这里只在**整列表**全部完成时按墙钟计时，
//! 到点把这段已完成任务从 dock 隐藏——**不删数据**，`/tasks` 依旧能看到、能操作。
//!
//! 回合制清理必须靠 `turn_start` 走字，而「最后一次完成之后 run 立刻结束」是常态，
//! 倒计时会冻在屏幕上；墙钟计时器不受此影响，避免完成列表长期占用 dock。
//!
//! 隐藏以时刻为界：隐藏那一刻之前就已完成的那些任务不再展示；此后新建、或从完成态被改回
//! 未完成的任务照常出现。只要列表里还有未完成任务，计时器就不启动（整列表口径，不是单任务）。

use super::types::{Task, TaskStatus};

/// 按墙钟隐藏整批已完成任务的计时器。
#[derive(Debug, Default)]
pub struct AutoHideManager {
    /// 列表进入「全部完成」的时刻（毫秒）；非全完成或已隐藏时为 `None`。
    all_completed_at: Option<u64>,
    /// 触发隐藏的时刻（毫秒）；隐藏此前已完成的那些任务。
    hidden_at: Option<u64>,
}

impl AutoHideManager {
    /// 用当前列表推进计时。返回是否**刚刚**触发隐藏（调用方据此补一次重绘把隐藏落到画面）。
    pub fn tick(&mut self, tasks: &[Task], now: u64, delay_ms: Option<u64>) -> bool {
        let Some(delay) = delay_ms else {
            self.reset();
            return false;
        };

        if tasks.is_empty() {
            self.reset();
            return false;
        }

        if tasks.iter().any(|t| t.status != TaskStatus::Completed) {
            self.all_completed_at = None;
            return false;
        }

        // 全完成、且都已在隐藏范围内：无需再计时。
        if tasks.iter().all(|t| self.hides(t)) {
            self.all_completed_at = None;
            return false;
        }

        match self.all_completed_at {
            None => {
                self.all_completed_at = Some(now);
                false
            }
            Some(at) if now.saturating_sub(at) >= delay => {
                self.hidden_at = Some(now);
                self.all_completed_at = None;
                true
            }
            _ => false,
        }
    }

    /// 立即隐藏当前的整批已完成任务，不等墙钟。
    ///
    /// 用于重新打开一个「任务已全部完成」的会话（`-c` / `/resume`）：那批任务是上次留下的
    /// 残影，不该再挂进 dock 等倒计时。列表为空、含未完成任务、或 `delay_ms` 为 `None`
    /// （配置为 `never`，即用户显式关掉自动隐藏）时都不动。
    pub fn hide_now(&mut self, tasks: &[Task], now: u64, delay_ms: Option<u64>) {
        if delay_ms.is_none() || tasks.is_empty() {
            return;
        }
        if tasks.iter().any(|t| t.status != TaskStatus::Completed) {
            return;
        }
        self.hidden_at = Some(now);
        self.all_completed_at = None;
    }

    /// 该任务是否应从 dock 隐藏：已完成、且在隐藏时刻之前就已完成。
    pub fn hides(&self, task: &Task) -> bool {
        task.status == TaskStatus::Completed && self.hidden_at.is_some_and(|h| task.updated_at <= h)
    }

    /// 是否正在等待隐藏（`wants_redraw` 用：倒计时期间需保持重绘才能到点触发）。
    pub fn is_pending(&self) -> bool {
        self.all_completed_at.is_some()
    }

    /// 清空全部状态（新会话 / 空列表 / 关闭本功能）。
    pub fn reset(&mut self) {
        self.all_completed_at = None;
        self.hidden_at = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str, status: TaskStatus, updated_at: u64) -> Task {
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
            created_at: updated_at,
            updated_at,
        }
    }

    #[test]
    fn hides_after_delay_only_when_all_completed() {
        let mut m = AutoHideManager::default();
        let tasks = vec![
            task("1", TaskStatus::Completed, 10),
            task("2", TaskStatus::Pending, 10),
        ];
        assert!(!m.tick(&tasks, 100, Some(1000)));
        assert!(!m.is_pending(), "有未完成任务不启动倒计时");

        let done = vec![task("1", TaskStatus::Completed, 10)];
        assert!(!m.tick(&done, 100, Some(1000)));
        assert!(m.is_pending());
        assert!(!m.tick(&done, 1099, Some(1000)), "未到点");
        assert!(m.tick(&done, 1100, Some(1000)), "到点触发隐藏");
        assert!(m.hides(&done[0]));
        assert!(!m.tick(&done, 5000, Some(1000)), "已隐藏不再重复触发");
    }

    #[test]
    fn never_never_hides() {
        let mut m = AutoHideManager::default();
        let done = vec![task("1", TaskStatus::Completed, 10)];
        assert!(!m.tick(&done, 10_000, None));
        assert!(!m.is_pending());
        assert!(!m.hides(&done[0]));
    }

    #[test]
    fn new_and_reactivated_tasks_stay_visible() {
        let mut m = AutoHideManager::default();
        let done = vec![task("1", TaskStatus::Completed, 10)];
        m.tick(&done, 100, Some(1000));
        m.tick(&done, 1100, Some(1000)); // 隐藏 #1

        // 新任务：旧的隐藏、新的可见
        let with_new = vec![
            task("1", TaskStatus::Completed, 10),
            task("2", TaskStatus::Pending, 2000),
        ];
        assert!(!m.tick(&with_new, 2000, Some(1000)));
        assert!(m.hides(&with_new[0]));
        assert!(!m.hides(&with_new[1]));

        // 旧任务被改回 in_progress：重新可见
        let reactivated = vec![task("2", TaskStatus::InProgress, 3000)];
        assert!(!m.tick(&reactivated, 3000, Some(1000)));
        assert!(!m.hides(&reactivated[0]));
    }

    #[test]
    fn new_batch_completion_hides_too() {
        let mut m = AutoHideManager::default();
        m.tick(&[task("1", TaskStatus::Completed, 10)], 100, Some(1000));
        m.tick(&[task("1", TaskStatus::Completed, 10)], 1100, Some(1000));

        // 新批次完成后重新计时，从而一并隐藏
        let batch = vec![
            task("1", TaskStatus::Completed, 10),
            task("3", TaskStatus::Completed, 5000),
        ];
        assert!(!m.tick(&batch, 5000, Some(1000)));
        assert!(m.is_pending(), "新完成的 #3 尚未被隐藏，需要重新计时");
        assert!(m.tick(&batch, 6000, Some(1000)));
        assert!(m.hides(&batch[1]));
    }

    #[test]
    fn hide_now_hides_only_all_completed_batches() {
        let done = vec![task("1", TaskStatus::Completed, 10)];
        let mixed = vec![
            task("1", TaskStatus::Completed, 10),
            task("2", TaskStatus::Pending, 10),
        ];

        // 配置为 never：不动。
        let mut m = AutoHideManager::default();
        m.hide_now(&done, 100, None);
        assert!(!m.hides(&done[0]));
        assert!(!m.is_pending());

        // 含未完成任务：不动。
        let mut m = AutoHideManager::default();
        m.hide_now(&mixed, 100, Some(1000));
        assert!(!m.hides(&mixed[0]));
        assert!(!m.hides(&mixed[1]));

        // 整列表完成：立即隐藏，且不再挂倒计时。
        let mut m = AutoHideManager::default();
        m.hide_now(&done, 100, Some(1000));
        assert!(m.hides(&done[0]));
        assert!(!m.is_pending());
        assert!(!m.tick(&done, 5000, Some(1000)), "已隐藏不再重复触发");
    }

    #[test]
    fn empty_list_resets() {
        let mut m = AutoHideManager::default();
        m.tick(&[task("1", TaskStatus::Completed, 10)], 100, Some(1000));
        m.tick(&[task("1", TaskStatus::Completed, 10)], 1100, Some(1000));
        m.tick(&[], 2000, Some(1000));
        assert!(!m.hides(&task("1", TaskStatus::Completed, 10)));
    }
}
