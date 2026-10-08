//! 进程级运行期开关：启动时确定一次，运行期全局只读。
//!
//! 为什么单独一个模块：这些开关的**判定方**（CLI/env，见 `main`）与**消费方**跨模块——
//! 离线模式同时被 `/download` 扩展（手动安装 fd/rg）与缓存保温读取。
//! 挂在任一消费方（如 `tools_manager`）都会让其余模块反向依赖它。

use std::sync::atomic::{AtomicBool, Ordering};

/// 离线模式（`--offline` 或 `PRUX_OFFLINE`），由 main 启动时设置一次。
///
/// 刻意不依赖进程环境变量：`--offline` 不会写 env，而扩展（`/download`）的
/// 下载判定需要读到同一份真源，故在内存里存一份真源。
static OFFLINE: AtomicBool = AtomicBool::new(false);

/// 启动时确定离线模式：`--offline` 或 `PRUX_OFFLINE`（真值：`1` / `true` / `yes`）。
///
/// 记录进程级真源并返回生效值——调用方仍需把它传给 `Agent` / TUI（那里的判定不是全局的，
/// 例如子代理与会话保温）。main 启动时调用一次。
pub fn init_offline(cli_flag: bool) -> bool {
    let env_flag = matches!(
        std::env::var("PRUX_OFFLINE").ok().as_deref().map(str::to_ascii_lowercase),
        Some(v) if v == "1" || v == "true" || v == "yes"
    );
    let offline = cli_flag || env_flag;
    OFFLINE.store(offline, Ordering::Relaxed);
    offline
}

/// 当前是否处于离线模式
pub fn offline() -> bool {
    OFFLINE.load(Ordering::Relaxed)
}

/// 直接设置离线模式（测试用；生产走 [`init_offline`]）
#[cfg(any(test, feature = "test-support"))]
pub fn set_offline(offline: bool) {
    OFFLINE.store(offline, Ordering::Relaxed);
}
