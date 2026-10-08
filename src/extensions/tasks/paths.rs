//! 会话任务文件的落点
//!
//! `session` 作用域把文件放在工作区里，一如往常；`session-global` 把它们放到
//! `<agent_dir>/extensions/tasks/sessions/<project-key>/` 下，与扩展自身的其余状态比邻，
//! 供希望仓库保持干净的人使用。
//!
//! 该选择**只决定新文件落在哪里**。已经持有工作区文件的会话在两种设置下都继续用它，
//! 因此选择 `session-global` 不搬移任何东西，切回来也不遗留任何东西。

use crate::{
    PROJECT_SCOPE_NAME,
    core::settings_manager::agent_dir,
    extensions::{tasks::config::TaskScope, util::project_key},
};
use std::path::{Path, PathBuf};

/// `session-global` 收集一个工作区的会话文件的位置：`<agent_dir>/extensions/tasks/sessions/<project-key>/`。
///
/// 每次调用都重新解析，绝不缓存：`agent_dir()` 会读环境。
pub fn global_session_tasks_dir(cwd: &str) -> PathBuf {
    agent_dir()
        .join("extensions")
        .join("tasks")
        .join("sessions")
        .join(project_key(cwd))
}

/// 工作区内位置，自 session 作用域引入以来未变。
pub fn workspace_session_task_file(cwd: &str, session_id: &str) -> PathBuf {
    Path::new(cwd)
        .join(PROJECT_SCOPE_NAME)
        .join("tasks")
        .join(format!("tasks-{session_id}.json"))
}

/// 支撑一个持久化会话的文件。
///
/// `session-global` 下仍**先查工作区**：已经在那里有文件的会话仍是那个文件的会话，
/// 读取它正是不需要任何迁移的全部理由。
pub fn session_task_file(cwd: &str, session_id: &str, scope: TaskScope) -> PathBuf {
    let in_workspace = workspace_session_task_file(cwd, session_id);
    if scope != TaskScope::SessionGlobal {
        return in_workspace;
    }
    if in_workspace.exists() {
        in_workspace
    } else {
        global_session_tasks_dir(cwd).join(format!("tasks-{session_id}.json"))
    }
}

/// 项目共享列表（`project` 作用域）。
pub fn project_task_file(cwd: &str) -> PathBuf {
    Path::new(cwd)
        .join(PROJECT_SCOPE_NAME)
        .join("tasks")
        .join("tasks.json")
}

/// 一个工作区的全局会话目录一旦空了就回收。
///
/// 只回收全局树。`<cwd>/.prux/tasks/` 即便空了也不动，因为迄今为止每个版本都这样，
/// 且 `.prux/` 里还放着不属于我们的项目配置。
pub fn reclaim_global_session_tasks_dir(cwd: &str) {
    _ = std::fs::remove_dir(global_session_tasks_dir(cwd));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::AgentDirGuard;

    #[test]
    fn session_global_dir_sits_under_agent_dir_extensions_tasks() {
        let _g = AgentDirGuard::temp();
        assert_eq!(
            global_session_tasks_dir("/Users/me/work/repo"),
            agent_dir()
                .join("extensions")
                .join("tasks")
                .join("sessions")
                .join("--Users-me-work-repo--")
        );
    }

    #[test]
    fn session_global_prefers_existing_workspace_file() {
        let _g = AgentDirGuard::temp();
        let cwd = tempfile::tempdir().unwrap();
        let cwd_s = cwd.path().to_string_lossy().to_string();

        // 无工作区文件 → 落全局
        let global = session_task_file(&cwd_s, "abc", TaskScope::SessionGlobal);
        assert!(global.starts_with(agent_dir()));

        // 有工作区文件 → 仍用工作区
        let ws = workspace_session_task_file(&cwd_s, "abc");
        std::fs::create_dir_all(ws.parent().unwrap()).unwrap();
        std::fs::write(&ws, "{}").unwrap();
        assert_eq!(
            session_task_file(&cwd_s, "abc", TaskScope::SessionGlobal),
            ws
        );

        // session 作用域恒用工作区
        assert_eq!(
            session_task_file(&cwd_s, "xyz", TaskScope::Session),
            workspace_session_task_file(&cwd_s, "xyz")
        );
    }

    #[test]
    fn reclaim_only_removes_empty_dir() {
        let _g = AgentDirGuard::temp();
        let cwd = tempfile::tempdir().unwrap();
        let cwd_s = cwd.path().to_string_lossy().to_string();
        let dir = global_session_tasks_dir(&cwd_s);
        std::fs::create_dir_all(&dir).unwrap();
        reclaim_global_session_tasks_dir(&cwd_s);
        assert!(!dir.exists());

        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("tasks-x.json"), "{}").unwrap();
        reclaim_global_session_tasks_dir(&cwd_s);
        assert!(dir.exists(), "非空目录不回收");
    }
}
