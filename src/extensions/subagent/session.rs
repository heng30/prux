//! 会话级缓存：当前会话的**任务目录键**与工作流的**让位判定**。
//!
//! 两件事放在一起是因为它们都是"本会话问一次就够、但会被多个入口读"的事实：
//!
//! 1. **任务目录键**：脚本落盘与 journal 都落在
//!    `$TMPDIR/prux-subagents-<uid>/<encoded-cwd>/<session>/tasks/` 下。
//!    现在 `on_session_start` 给 `Session`、`on_session_switched` 给路径，
//!    所以这里缓存下会话文件的 stem 当键——缺文件（不落盘的会话）时为 `None`，
//!    此时退化回不带会话子目录的老形状。
//! 2. **让位判定**：别的扩展已经提供了 `Workflow`/`workflow` 工具时，
//!    默认（`workflows = auto`）本扩展**整项功能让位**。判定只依赖"已注册扩展的工具名"，
//!    所以缓存一份：`filter_extension_tools` 会被 `compose_tools` 每次重建调多次，
//!    不能让它在里面反复扫注册表。
//!
//! 失效时机：`on_registered` / `on_session_start` / `on_enabled_changed` / 设置变更。
//! 惰性求值是兜底：若某个入口在失效之后、重算之前就读判定，
//! 会现场算一次（`registered()` 返回快照、释放锁，所以重入安全），但**不**在那里发通知。

use super::{EXT, config::WorkflowsMode, manager, util_notify};
use crate::core::{self, extensions::UiNotifyLevel};
use std::{
    path::Path,
    sync::{Mutex, OnceLock},
};

/// 别家编排扩展的工具名
///
/// 包含我们自己的名字（另一个扩展可能占了这个名字）、Claude Code 的裸 `Workflow`，
/// 以及它的小写形式（`@quintinshaw/pi-dynamic-workflows` 就是那个名字）。**精确匹配**：
/// `Workflow` 是工具名里很常见的词，子串匹配会静默关掉一个谁都没注意到的功能。
pub(crate) const FOREIGN_WORKFLOW_TOOL_NAMES: &[&str] =
    &["SubagentWorkflow", "Workflow", "workflow"];

/// 工作流是否可用（判定结果）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// 正常提供
    Available,
    /// 让位：别的扩展提供了工作流工具，我方收起（`workflows = auto`）
    StandDown,
    /// 用户关掉了工作流（`workflows = off`）
    Disabled,
}

impl Verdict {
    /// 是否应当从工具表里收起 `SubagentWorkflow`。
    pub(crate) fn withdrawn(self) -> bool {
        !matches!(self, Verdict::Available)
    }
}

/// 会话级缓存：任务目录键、工作流让位判定与通知去重标记。
#[derive(Default)]
struct State {
    /// 会话文件 stem（任务目录键）；None = 本会话没有落盘文件
    session_key: Option<String>,
    /// 已算出的让位判定（None = 需要重算）
    verdict: Option<Verdict>,
    /// 冲突通知是否已发过（每个判定代际一次）
    notified: bool,
}

/// 进程级会话作用域状态。
fn state() -> &'static Mutex<State> {
    /// 会话级缓存状态的进程级单例（惰性初始化）。
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(State::default()))
}

/// 在持有状态锁的临界区内运行 `f`（锁被毒化时取回内部值）。
fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> R {
    let mut st = state().lock().unwrap_or_else(|e| e.into_inner());
    f(&mut st)
}

/// 记下当前会话的任务目录键（`None` = 无落盘文件）。
pub(crate) fn set_session_key(key: Option<String>) {
    with_state(|st| st.session_key = key);
}

/// 由会话文件路径推出任务目录键（文件名 stem，形如 `<ts>_<id>`）。
pub(crate) fn key_from_path(path: &str) -> Option<String> {
    Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

/// 当前会话的任务目录键。
pub(crate) fn session_key() -> Option<String> {
    with_state(|st| st.session_key.clone())
}

/// 失效缓存的判定（注册变化 / 会话切换 / 扩展启用状态变化 / 设置变更后调用）。
pub(crate) fn invalidate() {
    with_state(|st| {
        st.verdict = None;
        st.notified = false;
    });
}

/// 清空全部会话级缓存（会话切换 / 扩展禁用）。
pub(crate) fn reset() {
    with_state(|st| {
        st.session_key = None;
        st.verdict = None;
        st.notified = false;
    });
}

/// 现场计算让位判定（不缓存、不发通知）。
///
/// `foreign` = 除本扩展以外，提供 [`FOREIGN_WORKFLOW_TOOL_NAMES`] 里任一工具的扩展名（用于消息）。
/// 上游靠"工具描述不等于我自己的描述"认出自己；有 `Extension::name()`，直接按名字排除更干净。
pub(crate) fn compute() -> (Verdict, Option<String>) {
    let mode = manager::config().workflows;
    if mode == WorkflowsMode::Off {
        return (Verdict::Disabled, None);
    }

    let foreign = with_state_stranger();
    match (mode, foreign) {
        (_, Some(ext)) => {
            if mode == WorkflowsMode::On {
                (Verdict::Available, Some(ext))
            } else {
                (Verdict::StandDown, Some(ext))
            }
        }
        _ => (Verdict::Available, None),
    }
}

/// 找出提供工作流工具的**别的**扩展名（精确名字匹配，避免误伤 `github_workflow_run` 之类）。
fn with_state_stranger() -> Option<String> {
    for ext in core::extensions::registered() {
        if ext.name() == EXT {
            continue;
        }

        if ext
            .tools()
            .iter()
            .any(|t| FOREIGN_WORKFLOW_TOOL_NAMES.contains(&t.name.as_str()))
        {
            return Some(ext.name().to_string());
        }
    }
    None
}

/// 当前让位判定（惰性求值 + 缓存）。
pub(crate) fn verdict() -> Verdict {
    if let Some(v) = with_state(|st| st.verdict) {
        return v;
    }

    let (verdict, _) = compute();
    with_state(|st| st.verdict = Some(verdict));
    verdict
}

/// 首次算出"让位/关闭"时给出一次性说明。
///
/// 从 `on_registered` / `on_session_start` / `on_enabled_changed` 调用（那些入口在注册表锁外），
/// 不在 [`filter_extension_tools`](crate::core::extensions::Extension::filter_extension_tools)
/// 的渲染路径里调用。
pub(crate) fn announce_if_needed() {
    let (verdict, stranger) = compute();
    with_state(|st| st.verdict = Some(verdict));

    if verdict == Verdict::Available {
        return;
    }

    let already = with_state(|st| std::mem::replace(&mut st.notified, true));
    if already {
        return;
    }

    let msg = match (verdict, stranger.as_deref()) {
        (Verdict::StandDown, Some(ext)) => format!(
            "Another extension ({ext}) already provides a workflow tool, so this extension's \
             workflows are disabled for this session to avoid offering the model two rchestrators. \
             Set \"workflows\": \"on\" in the subagent extension config to keep both."
        ),
        (Verdict::Disabled, _) => {
            "subagent: workflows are off (`workflows: off`); SubagentWorkflow is not offered"
                .to_string()
        }
        (Verdict::Available, Some(ext)) => format!(
            "Another extension ({ext}) also provides a workflow tool. Both are offered (`workflows: on`); \
            disable one of the two if the model picks the wrong one."
        ),
        (Verdict::Available, None) | (Verdict::StandDown, None) => return,
    };

    util_notify(&msg, UiNotifyLevel::Warning);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_from_path_uses_the_file_stem() {
        assert_eq!(
            key_from_path("/tmp/sessions/2026-02-14T12-30-00-000Z_ab12cd34.jsonl").as_deref(),
            Some("2026-02-14T12-30-00-000Z_ab12cd34")
        );
        assert_eq!(key_from_path(""), None);
    }

    #[test]
    fn reset_clears_both_caches() {
        set_session_key(Some("s1".into()));
        with_state(|st| {
            st.verdict = Some(Verdict::StandDown);
            st.notified = true;
        });
        reset();
        assert_eq!(session_key(), None);
        assert_eq!(with_state(|st| st.verdict), None);
        assert!(!with_state(|st| st.notified));
    }

    #[test]
    fn foreign_tool_names_match_exactly() {
        assert!(FOREIGN_WORKFLOW_TOOL_NAMES.contains(&"Workflow"));
        assert!(FOREIGN_WORKFLOW_TOOL_NAMES.contains(&"workflow"));
        assert!(FOREIGN_WORKFLOW_TOOL_NAMES.contains(&"SubagentWorkflow"));
        // 精确匹配，不误伤常见词
        assert!(!FOREIGN_WORKFLOW_TOOL_NAMES.contains(&"github_workflow_run"));
        assert!(!FOREIGN_WORKFLOW_TOOL_NAMES.contains(&"list_workflows"));
    }
}
