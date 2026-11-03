//! 在同步上下文里发起异步任务：优先用当前 tokio 运行时，没有就借一个进程级运行时。
//!
//! 扩展的斜杠命令与事件钩子都是同步接口。TUI 里它们总在 tokio 运行时内运行，桌面 GUI 里
//! 命令跑在视图线程上（没有运行时），于是 `/btw`、`/recap`、`/doctor` 的网络请求全部退化成
//! 「no async runtime」。这里给这类一次性任务一个统一的去处：调用方只关心"有没有跑起来"。

use std::sync::OnceLock;
use tokio::runtime::Handle;

/// 宿主注册的运行时句柄（GUI 启动时登记，TUI 用当前线程的运行时，不需要它）。
static INSTALLED: OnceLock<Handle> = OnceLock::new();

/// 登记宿主的 tokio 运行时：不在运行时内的线程（GUI 视图线程）也能发起后台任务。
///
/// 只接受首次登记（`OnceLock` 语义），重复调用忽略。
pub fn install(handle: Handle) {
    _ = INSTALLED.set(handle);
}

/// 取一个可用的运行时句柄：当前线程在 tokio 运行时内就用它，否则用宿主登记的。
///
/// 两者都没有时返回 `None`，调用方据此给出提示（单测里就是这种情况）。
pub fn handle() -> Option<Handle> {
    Handle::try_current()
        .ok()
        .or_else(|| INSTALLED.get().cloned())
}

/// 发起一个后台任务；返回 `false` 表示拿不到运行时（任务没跑起来）。
///
/// 任务里的输出由调用方自己送到 UI（`request_ui`），这里只负责让它跑起来。
pub fn spawn<F>(future: F) -> bool
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    match handle() {
        Some(handle) => {
            handle.spawn(future);
            true
        }
        None => false,
    }
}

/// 在同步线程上把一个 future 跑完并拿回结果（阻塞当前线程，直到它结束）。
///
/// 与 [`spawn`] 是同一件事的两面：那里只负责发出去，这里负责等结果——给“流程本身是同步的、
/// 中间必须问一次模型或一次网络”的调用方（扩展的后台工作线程）用。
///
/// 不复用 [`handle`]：那里给的句柄可能属于当前线程所在的运行时，在运行时内阻塞会 panic；
/// 自建一个单线程运行时反正只花微秒，而调用方本来就没有运行时上下文。
///
/// 返回 `Err` 只表示运行时建不起来（资源耗尽），不是 future 本身的失败——future 的输出
/// 原样交回调用方。
pub fn run_blocking<F: std::future::Future>(future: F) -> Result<F::Output, String> {
    Ok(tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| format!("tokio: {err}"))?
        .block_on(future))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_blocking_returns_the_future_output() {
        assert_eq!(run_blocking(async { 7u32 }).unwrap(), 7);
    }

    #[test]
    fn run_blocking_works_on_a_thread_without_a_runtime() {
        // 扩展的后台工作线程就是这种环境：`Handle::try_current()` 拿不到东西。
        let out = std::thread::spawn(|| run_blocking(async { "done" }).unwrap())
            .join()
            .unwrap();
        assert_eq!(out, "done");
    }
}
