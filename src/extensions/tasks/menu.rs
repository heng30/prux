//! `/tasks` 命令的交互状态机
//!
//! 面板选项一律按 `<key>  <展示文本>` 组装（与 `/agents`、`/download` 同一约定）：
//! 回传串的首个 token 是稳定 key，分发只看它，展示文案与字形怎么改都不影响匹配。

use super::{
    EXT,
    store::TaskStore,
    types::{Task, TaskStatus, TaskStatusOrDeleted, TaskUpdateFields},
};
use crate::{
    core::extensions::{self, ExtensionUiRequest, SelectOption},
    extensions::util::choice_key,
    utils::glyphs::{
        DEF_ARROW_LEFT, DEF_COMPLETED, DEF_DONE, DEF_FAILED, DEF_IN_PROGRESS, DEF_PENDING, DEF_PLAY,
    },
};
use std::fmt::Display;

/// `/tasks` 交互面板的菜单层级语义，决定回传串该分发到哪一层。
#[derive(Debug, Clone)]
enum MenuKind {
    /// 主菜单。
    Main,
    /// 任务列表菜单。
    Tasks,
    /// 单个任务的行动菜单（携带任务 id）。
    TaskActions(String),
}

/// 当前挂起的 Select 及其语义。
#[derive(Debug, Default)]
pub struct MenuState {
    /// 挂起中的请求 id 与其菜单语义；无挂起时为 `None`。
    pending: Option<(u64, MenuKind)>,
}

/// 菜单项 key：选项串 `<key>  <展示文本>` 的首个 token。
///
/// 构造与分发共用同一常量，故改展示文案、换字形、调宽度都不会让分发静默失效
/// （早先直接用含字形的标签做匹配，字形一改匹配就断）。
mod keys {
    /// 「查看全部任务」菜单项的稳定 key。
    pub const VIEW: &str = "view";
    /// 「清除已完成任务」菜单项的稳定 key。
    pub const CLEAR_COMPLETED: &str = "clear-completed";
    /// 「清除全部任务」菜单项的稳定 key。
    pub const CLEAR_ALL: &str = "clear-all";
    /// 「打开本扩展设置」菜单项的稳定 key。
    pub const SETTINGS: &str = "settings";
    /// 「返回上一层菜单」菜单项的稳定 key，各层菜单共用。
    pub const BACK: &str = "back";
    /// 「把任务置为进行中」操作项的稳定 key。
    pub const START: &str = "start";
    /// 「把任务标记为完成」操作项的稳定 key。
    pub const COMPLETE: &str = "complete";
    /// 「删除任务」操作项的稳定 key。
    pub const DELETE: &str = "delete";
    /// 任务行 key 的前缀：`task:<id>`。id 由 store 给出，不从自由文本标签里解析。
    pub const TASK: &str = "task:";
}

/// 组装一个菜单项：`<key>  <展示文本>`（默认配色）。
fn option(key: &str, label: impl Display) -> SelectOption {
    SelectOption::new(format!("{key}  {label}"))
}

/// 组装一个带展示色的菜单项：色为主题键（见 [`row_fg`]）或 hex。
fn colored_option(key: &str, fg: &str, label: impl Display) -> SelectOption {
    SelectOption::styled(format!("{key}  {label}"), fg)
}

/// 「返回」项：各层菜单共用。
fn back_option() -> SelectOption {
    option(keys::BACK, format!("{DEF_ARROW_LEFT} Back"))
}

/// 任务行：`task:<id>  <字形> #<id> [status] <subject>`，按状态着色。
fn task_option(task: &Task) -> SelectOption {
    colored_option(
        &format!("{}{}", keys::TASK, task.id),
        row_fg(task.status),
        row_label(task),
    )
}

/// 任务行的展示色（与常驻 widget 的状态配色一致）：
/// `completed` = success（绿）、`in_progress` = accent（青）、`pending` = 终端默认前景。
fn row_fg(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Completed => "success",
        TaskStatus::InProgress => "accent",
        TaskStatus::Pending => "text",
    }
}

/// 发一个 Select 请求并记下语义，返回请求 id。
fn select(menu: &mut MenuState, kind: MenuKind, title: String, options: Vec<SelectOption>) -> u64 {
    let id = extensions::next_ui_id();
    menu.pending = Some((id, kind));
    extensions::request_ui(ExtensionUiRequest::Select { id, title, options });
    id
}

/// 打开主菜单。
pub fn open_main(store: &mut TaskStore, menu: &mut MenuState) {
    let tasks = store.list(None);
    let total = tasks.len();
    let completed = tasks
        .iter()
        .filter(|t| t.status == TaskStatus::Completed)
        .count();

    let mut choices = vec![option(keys::VIEW, format!("View all tasks ({total})"))];

    if completed > 0 {
        choices.push(option(
            keys::CLEAR_COMPLETED,
            format!("Clear completed ({completed})"),
        ));
    }

    if total > 0 {
        choices.push(option(keys::CLEAR_ALL, format!("Clear all ({total})")));
    }

    choices.push(option(keys::SETTINGS, "Settings"));

    select(menu, MenuKind::Main, "Tasks".to_string(), choices);
}

/// 处理一次 Select 回传。返回 `true` 表示 store 被改动（调用方需刷新展示）。
pub fn on_choice(
    store: &mut TaskStore,
    menu: &mut MenuState,
    id: u64,
    choice: Option<String>,
) -> bool {
    let Some((pending_id, kind)) = menu.pending.clone() else {
        return false;
    };

    if pending_id != id {
        return false;
    }
    menu.pending = None;

    let Some(choice) = choice else {
        return false; // Esc 取消
    };

    let key = choice_key(&choice);
    match kind {
        MenuKind::Main => match key {
            keys::VIEW => {
                open_tasks_list(store, menu);
                false
            }
            keys::SETTINGS => {
                extensions::request_ui(ExtensionUiRequest::ShowSettings {
                    ext: EXT.to_string(),
                });
                false
            }
            keys::CLEAR_COMPLETED => {
                _ = store.clear_completed();
                open_main(store, menu);
                true
            }
            keys::CLEAR_ALL => {
                _ = store.clear_all();
                open_main(store, menu);
                true
            }
            // 空列表时 `open_tasks_list` 用 Main 面板只给一个 Back，这里必须认领，
            // 否则按了没反应（面板关掉、菜单也没回来）。
            keys::BACK => {
                open_main(store, menu);
                false
            }
            _ => false,
        },
        MenuKind::Tasks => {
            if key == keys::BACK {
                open_main(store, menu);
            } else if let Some(task_id) = key.strip_prefix(keys::TASK) {
                open_task_actions(store, menu, task_id);
            } else {
                open_tasks_list(store, menu);
            }

            false
        }
        MenuKind::TaskActions(task_id) => {
            let mut changed = false;
            match key {
                keys::START => {
                    _ = store.update(
                        &task_id,
                        TaskUpdateFields {
                            status: Some(TaskStatusOrDeleted::Status(TaskStatus::InProgress)),
                            ..Default::default()
                        },
                    );
                    changed = true;
                }
                keys::COMPLETE => {
                    _ = store.update(
                        &task_id,
                        TaskUpdateFields {
                            status: Some(TaskStatusOrDeleted::Status(TaskStatus::Completed)),
                            ..Default::default()
                        },
                    );
                    changed = true;
                }
                keys::DELETE => {
                    _ = store.update(
                        &task_id,
                        TaskUpdateFields {
                            status: Some(TaskStatusOrDeleted::Deleted),
                            ..Default::default()
                        },
                    );
                    changed = true;
                }
                _ => {}
            }
            open_tasks_list(store, menu);
            changed
        }
    }
}

/// 打开任务列表菜单；无任务时显示 "No tasks" 占位，末尾始终附带返回项。
fn open_tasks_list(store: &mut TaskStore, menu: &mut MenuState) {
    let tasks = store.list(None);
    if tasks.is_empty() {
        select(
            menu,
            MenuKind::Main,
            "No tasks".to_string(),
            vec![back_option()],
        );
        return;
    }

    let mut choices: Vec<SelectOption> = tasks.iter().map(task_option).collect();
    choices.push(back_option());
    select(menu, MenuKind::Tasks, "Tasks".to_string(), choices);
}

/// 打开单个任务的操作菜单（按当前状态给出 Start / Complete / Delete）。
/// 任务已不存在（面板打开期间被清掉）时退回任务列表，避免菜单悬空。
fn open_task_actions(store: &mut TaskStore, menu: &mut MenuState, task_id: &str) {
    let Some(task) = store.get(task_id) else {
        // 面板打开期间任务被清掉（clear / 另一个终端）：退回列表，别让菜单悬空。
        open_tasks_list(store, menu);
        return;
    };

    let mut actions: Vec<SelectOption> = Vec::new();
    if task.status == TaskStatus::Pending {
        actions.push(option(
            keys::START,
            format!("{DEF_PLAY} Start (in_progress)"),
        ));
    }

    if task.status == TaskStatus::InProgress {
        actions.push(option(keys::COMPLETE, format!("{DEF_DONE} Complete")));
    }

    actions.push(option(keys::DELETE, format!("{DEF_FAILED} Delete")));
    actions.push(back_option());

    let title = format!(
        "#{} [{}] {}\n{}",
        task.id,
        task.status.as_str(),
        task.subject,
        task.description
    );
    select(menu, MenuKind::TaskActions(task.id.clone()), title, actions);
}

/// 任务列表行的展示文本：状态图标 + `#id [status] subject`。
fn row_label(task: &Task) -> String {
    let glyph = match task.status {
        TaskStatus::Completed => DEF_COMPLETED,
        TaskStatus::InProgress => DEF_IN_PROGRESS,
        TaskStatus::Pending => DEF_PENDING,
    };

    format!(
        "{glyph} #{} [{}] {}",
        task.id,
        task.status.as_str(),
        task.subject
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::AUTH_TEST_LOCK;

    /// 面板队列是全局共享的，其它并行测试也会往里塞请求：
    /// 持全局锁串行，并扫描（而非取队首）出本测试的 Select。
    struct Env {
        /// 持锁到测试结束（Drop 时顺带清空队列）。
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    impl Env {
        fn new() -> Self {
            let guard = AUTH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            drain();
            Self { _guard: guard }
        }
    }

    impl Drop for Env {
        fn drop(&mut self) {
            drain();
        }
    }

    fn drain() {
        while extensions::take_pending_ui().is_some() {}
    }

    /// 取走下一个 Select：标题 + 选项**回传串**（多数测试只关心 key/文案，不看颜色）。
    fn next_select() -> (u64, String, Vec<String>) {
        let (id, title, options) = next_select_styled();
        (id, title, options.into_iter().map(|o| o.text).collect())
    }

    /// 取走下一个 Select，保留逐项展示色（逐行配色断言用）。
    fn next_select_styled() -> (u64, String, Vec<SelectOption>) {
        while let Some(req) = extensions::take_pending_ui() {
            if let ExtensionUiRequest::Select { id, title, options } = req {
                return (id, title, options);
            }
        }
        panic!("expected a Select request");
    }

    fn mem_with(subjects: &[&str]) -> TaskStore {
        let mut store = TaskStore::new(None);
        for s in subjects {
            store
                .create(s.to_string(), "d".to_string(), None, None)
                .unwrap();
        }
        store
    }

    /// 空列表只给一个 Back，且必须被 Main 面板认领：
    /// 旧实现落到 `_ => false`，面板关掉、主菜单也不回来（按了没反应）。
    #[test]
    fn empty_list_back_reopens_main_menu() {
        let _env = Env::new();
        let mut store = TaskStore::new(None);
        let mut menu = MenuState::default();

        open_tasks_list(&mut store, &mut menu);
        let (id, title, options) = next_select();
        assert_eq!(title, "No tasks");
        assert_eq!(options.len(), 1, "{options:?}");
        assert_eq!(choice_key(&options[0]), keys::BACK);

        assert!(!on_choice(
            &mut store,
            &mut menu,
            id,
            Some(options[0].clone())
        ));
        let (_, title, options) = next_select();
        assert_eq!(title, "Tasks", "Back 应回到主菜单");
        assert!(options.iter().any(|o| choice_key(o) == keys::VIEW));
    }

    /// 选项 key 是稳定标识：任务行带 `task:<id>`，字形只出现在展示部分。
    /// 把字形换成别的字符后分发必须照旧（旧实现按含字形的标签匹配，一改就断）。
    #[test]
    fn task_action_dispatches_by_key_not_by_glyph() {
        let _env = Env::new();
        let mut store = mem_with(&["A"]);
        let mut menu = MenuState::default();

        open_tasks_list(&mut store, &mut menu);
        let (id, _, options) = next_select();
        let row = options
            .iter()
            .find(|o| choice_key(o) == "task:1")
            .expect("任务行 key 应带 id");
        assert!(!on_choice(&mut store, &mut menu, id, Some(row.clone())));

        let (id, _, options) = next_select();
        let start = options
            .iter()
            .find(|o| choice_key(o) == keys::START)
            .expect("pending 任务应广告 Start");
        assert!(start.contains(DEF_PLAY), "展示部分用 glyphs 里的字形");

        let mangled = start.replace(DEF_PLAY, "?");
        assert!(
            on_choice(&mut store, &mut menu, id, Some(mangled)),
            "字形变了也该分发到 start"
        );
        assert_eq!(store.get("1").unwrap().status, TaskStatus::InProgress);
    }

    /// 主菜单的清理项按 key 分发，并把 store 改动如实上报给调用方。
    #[test]
    fn clear_completed_from_main_menu_reports_change() {
        let _env = Env::new();
        let mut store = mem_with(&["A", "B"]);
        store
            .update(
                "1",
                TaskUpdateFields {
                    status: Some(TaskStatusOrDeleted::Status(TaskStatus::Completed)),
                    ..Default::default()
                },
            )
            .unwrap();
        let mut menu = MenuState::default();

        open_main(&mut store, &mut menu);
        let (id, _, options) = next_select();
        let clear = options
            .iter()
            .find(|o| choice_key(o) == keys::CLEAR_COMPLETED)
            .expect("有已完成任务就该广告清理项");

        assert!(on_choice(&mut store, &mut menu, id, Some(clear.clone())));
        assert_eq!(store.list(None).len(), 1);
    }

    /// 主菜单不再广告 `Create task`：建任务只能走 `/tasks create <subject>` 子命令。
    /// 面板条目少一项，且旧 key（面板里残留的 `create` 回传）不被认领、不动 store。
    #[test]
    fn main_menu_no_longer_advertises_create() {
        let _env = Env::new();
        let mut store = TaskStore::new(None);
        let mut menu = MenuState::default();

        open_main(&mut store, &mut menu);
        let (id, _, options) = next_select();
        let advertised: Vec<&str> = options.iter().map(|o| choice_key(o)).collect();
        assert_eq!(advertised, [keys::VIEW, keys::SETTINGS], "{options:?}");

        assert!(!on_choice(
            &mut store,
            &mut menu,
            id,
            Some("create  Create task".to_string())
        ));
        assert!(store.list(None).is_empty(), "旧 create key 不得建任务");
    }

    /// 任务行按状态着色（与常驻 widget 一致）：completed = success、in_progress = accent、
    /// pending = text（终端默认前景）；返回项等非任务行不着色（跟面板默认配色）。
    #[test]
    fn task_rows_are_colored_by_status() {
        let _env = Env::new();
        let mut store = mem_with(&["A", "B", "C"]);
        for (id, status) in [("1", TaskStatus::Completed), ("2", TaskStatus::InProgress)] {
            store
                .update(
                    id,
                    TaskUpdateFields {
                        status: Some(TaskStatusOrDeleted::Status(status)),
                        ..Default::default()
                    },
                )
                .unwrap();
        }
        let mut menu = MenuState::default();

        open_tasks_list(&mut store, &mut menu);
        let (_, _, options) = next_select_styled();
        let fg = |key: &str| {
            options
                .iter()
                .find(|o| choice_key(&o.text) == key)
                .and_then(|o| o.fg.clone())
        };
        assert_eq!(fg("task:1").as_deref(), Some("success"));
        assert_eq!(fg("task:2").as_deref(), Some("accent"));
        assert_eq!(fg("task:3").as_deref(), Some("text"));
        assert_eq!(fg(keys::BACK), None, "返回项不该着色");
    }

    /// 旧 id（面板已被别的请求顶掉）不得再改动 store。
    #[test]
    fn stale_id_is_ignored() {
        let _env = Env::new();
        let mut store = mem_with(&["A"]);
        let mut menu = MenuState::default();

        open_main(&mut store, &mut menu);
        let (id, _, _) = next_select();

        let stale = format!("{}  {DEF_FAILED} Delete", keys::DELETE);
        assert!(!on_choice(
            &mut store,
            &mut menu,
            id.wrapping_add(1),
            Some(stale)
        ));
        assert_eq!(store.list(None).len(), 1, "旧 id 不应触发任何动作");
    }
}
