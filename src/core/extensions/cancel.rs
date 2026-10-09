//! 后台可取消工作的登记处。
//!
//! 扩展把「用户应当能取消的后台工作」登记进来（目前是 MCP OAuth 登录），
//! UI 在 Esc / Ctrl+C 与会话关闭时统一取消，不必知道具体是哪个扩展。

use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
};
use tokio_util::sync::CancellationToken;

/// 全局登记表。
static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();

/// 已登记的后台工作：自增 id → (标签, 取消信号)。
#[derive(Default)]
struct Registry {
    /// 下一个可用 id（只增不减，避免与尚未摘除的条目撞号）。
    next_id: u64,
    /// 在飞条目。
    entries: HashMap<u64, (String, CancellationToken)>,
}

/// 取得全局登记表（首次调用时初始化）。
fn registry() -> &'static Mutex<Registry> {
    REGISTRY.get_or_init(|| Mutex::new(Registry::default()))
}

/// 一项后台工作的登记凭据；drop 时自动从登记表摘除。
///
/// 忘记 drop（例如任务 panic 前未归还）只会让条目留在表里，取消仍然安全：
/// [`cancel_background_work`] 对一个已结束工作的 token 取消是空操作。
#[must_use = "guard 必须存活到后台工作结束"]
pub struct BackgroundCancelGuard {
    /// 条目 id。
    id: u64,
}

impl Drop for BackgroundCancelGuard {
    /// 摘除本条目（工作已结束，不该再被取消）。
    fn drop(&mut self) {
        registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entries
            .remove(&self.id);
    }
}

/// 登记一项可被用户取消的后台工作。
///
/// `label`：用于诊断/日志的工作名（如 `mcp-login:docs-server`）。
/// `token`：该工作的取消信号；UI 触发取消时会 `cancel()` 它。
/// 返回的 guard 必须存活到工作结束（drop 即摘除登记）。
pub fn register_background_cancel(
    label: impl Into<String>,
    token: CancellationToken,
) -> BackgroundCancelGuard {
    let mut table = registry().lock().unwrap_or_else(|e| e.into_inner());
    let id = table.next_id;
    table.next_id += 1;
    table.entries.insert(id, (label.into(), token));
    BackgroundCancelGuard { id }
}

/// 取消所有已登记的后台工作（Esc / Ctrl+C 中断、会话关闭、退出时调用）。
///
/// 返回被取消的条目数（0 表示当时没有在飞的后台工作）。
pub fn cancel_background_work() -> usize {
    let table = registry().lock().unwrap_or_else(|e| e.into_inner());
    for (_, token) in table.entries.values() {
        token.cancel();
    }
    table.entries.len()
}

/// 当前在飞的后台工作数（UI 判断「Esc 是否该先取消后台工作」）。
pub fn background_work_count() -> usize {
    registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entries
        .len()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 登记 → 取消 → guard drop 后摘除。
    /// 登记表是进程全局的：与其它碰它的用例串行（见 AGENTS.md 的全局注册表约定）。
    #[test]
    fn register_cancel_and_unregister() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let before = background_work_count();
        let token = CancellationToken::new();
        let guard = register_background_cancel("test-job", token.clone());
        assert_eq!(background_work_count(), before + 1);
        assert!(!token.is_cancelled());

        assert_eq!(cancel_background_work(), before + 1);
        assert!(token.is_cancelled(), "取消后 token 必须已取消");

        drop(guard);
        assert_eq!(background_work_count(), before, "guard drop 后应摘除");
    }

    /// 取消一个已经结束（guard 已 drop）的工作不报错、也不影响其它条目。
    #[test]
    fn cancelling_after_drop_is_a_no_op() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let before = background_work_count();
        let done = register_background_cancel("done-job", CancellationToken::new());
        drop(done);

        let live_token = CancellationToken::new();
        let live = register_background_cancel("live-job", live_token.clone());
        let _ = cancel_background_work();
        assert!(live_token.is_cancelled());
        drop(live);
        assert_eq!(background_work_count(), before);
    }
}
