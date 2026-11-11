//! 项目信任的 CLI 决策
//! 信任状态本身由 core/project_trust.rs 管理，这里只做启动期决策。

use crate::core::project_trust::{
    has_trust_requiring_project_resources, is_project_trusted, set_session_trust, trust_decision,
};
use std::path::Path;

/// 启动时决定项目是否被信任。
/// -a/--approve → true；--no-approve → false；否则已保存的决策优先，
/// 没有任何决策时才把「无需信任的项目」自动视为信任。
///
/// 决策同时登记为**本次运行的会话信任**（`set_session_trust`）：后续所有
/// `is_project_trusted` 调用（项目 SYSTEM.md/context、子代理的
/// agents/workflows/agent-memory/config）都必须看到同一个启动结论，
/// 否则 `-a/--approve` 或启动解析出的信任在 worker 线程上会凭空消失。
pub fn resolve_trusted(cwd: &Path, agent_dir: &Path, approve: Option<bool>) -> bool {
    let trusted = match approve {
        Some(trusted) => trusted,
        // 用户明确保存过「不信任」时不得被「无需信任的资源」短路掉：
        // 否则 trust.json 的 false 在运行期被 is_project_trusted 静默丢弃。
        None if trust_decision(cwd, agent_dir).is_some() => is_project_trusted(cwd, agent_dir),
        None if !has_trust_requiring_project_resources(cwd) => true,
        None => is_project_trusted(cwd, agent_dir),
    };

    set_session_trust(cwd, trusted);
    trusted
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PROJECT_SCOPE_NAME, core::project_trust::clear_session_trust};

    #[test]
    fn resolve_trusted_registers_session_trust() {
        // 启动结论必须立即对所有 is_project_trusted 调用可见（而不是只写进 App 的
        // reload_ctx.trusted）：子代理资源加载跑在 worker 线程，读不到 App 状态。
        let _ad = crate::test_support::AgentDirGuard::temp();
        let agent_dir = crate::core::settings_manager::agent_dir();
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("proj");
        std::fs::create_dir_all(cwd.join(PROJECT_SCOPE_NAME)).unwrap();
        std::fs::write(cwd.join(PROJECT_SCOPE_NAME).join("settings.json"), "{}").unwrap();

        assert!(resolve_trusted(&cwd, &agent_dir, Some(true)));
        assert!(
            is_project_trusted(&cwd, &agent_dir),
            "-a/--approve 应对 is_project_trusted 立即生效"
        );

        assert!(!resolve_trusted(&cwd, &agent_dir, Some(false)));
        assert!(
            !is_project_trusted(&cwd, &agent_dir),
            "--no-approve 应立即生效"
        );

        clear_session_trust(&cwd);
        assert!(
            !is_project_trusted(&cwd, &agent_dir),
            "清掉会话决策后：无 trust.json 条目 + 默认 Ask → 未信任"
        );
    }

    #[test]
    fn resolve_trusted_without_requiring_resources_is_trusted() {
        // 无受信任门控资源时不询问也不门控（对齐 pi），但仍登记会话信任。
        let _ad = crate::test_support::AgentDirGuard::temp();
        let agent_dir = crate::core::settings_manager::agent_dir();
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("proj");
        std::fs::create_dir_all(&cwd).unwrap();

        assert!(resolve_trusted(&cwd, &agent_dir, None));
        assert!(is_project_trusted(&cwd, &agent_dir));
        clear_session_trust(&cwd);
    }

    #[test]
    fn saved_distrust_beats_auto_trust() {
        // pi #10947：trust.json 里显式「不信任」优先于「无需信任资源就自动信任」。
        let _ad = crate::test_support::AgentDirGuard::temp();
        let agent_dir = crate::core::settings_manager::agent_dir();
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("proj");
        std::fs::create_dir_all(&cwd).unwrap();
        // 只有 AGENTS.md：不需要信任门控资源
        std::fs::write(cwd.join("AGENTS.md"), "hi").unwrap();
        crate::core::project_trust::set_project_trust(&cwd, &agent_dir, false);

        assert!(
            !resolve_trusted(&cwd, &agent_dir, None),
            "已保存的「不信任」应生效"
        );
        assert!(!is_project_trusted(&cwd, &agent_dir));
        clear_session_trust(&cwd);
        // -a 仍然可以本次覆盖
        assert!(resolve_trusted(&cwd, &agent_dir, Some(true)));
        clear_session_trust(&cwd);
    }
}
