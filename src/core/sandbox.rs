//! OS 级命令沙箱接缝：bash 工具启动子进程前经此改写命令行。
//!
//! 具体沙箱（Linux `bwrap` / macOS `sandbox-exec`）由 `sandbox` 扩展装进来；`core` 不能依赖
//! `extensions`，所以这里只放一张全局包装器表（0 或 1 个条目），不解释任何沙箱语义。
//!
//! 三条约定：
//! - 包装器按**每次调用**的工作目录构造参数（可写目录随 cwd 变），因此登记的是闭包而不是静态命令行；
//! - 包装器返回 `Err` 表示**拒绝执行**这条命令，调用方必须把原因原样交回模型，绝不能退回无沙箱执行；
//! - 读写 [`current`] 都不在锁内回调任何东西（闭包 `Arc` 克隆出来再调）。

use std::{
    path::Path,
    sync::{Arc, Mutex, OnceLock},
};

/// 命令包装器：给定工作目录，给出启动方式；`Err` 是拒绝执行的原因（面向模型/用户）。
pub type ShellWrapper = Arc<dyn Fn(&Path) -> Result<SandboxedCommand, String> + Send + Sync>;

/// 当前登记的包装器；`None` = 不加包装，命令直接执行。
static WRAPPER: OnceLock<Mutex<Option<ShellWrapper>>> = OnceLock::new();

/// 包装后的启动方式：先跑 `program`，`prefix_args` 之后接原来的 shell 与 `-c <命令>`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxedCommand {
    /// 实际启动的程序（`bwrap` / `sandbox-exec` / 测试替身）。
    pub program: String,
    /// 追加在 `program` 之后的固定参数；真正的 shell 路径与 `-c <命令>` 由调用方接在后面。
    pub prefix_args: Vec<String>,
}

/// 包装器表的叶级锁。
fn slot() -> &'static Mutex<Option<ShellWrapper>> {
    WRAPPER.get_or_init(|| Mutex::new(None))
}

/// 登记包装器（重复调用替换旧的）。扩展在「启用且后端可用」时调用。
pub fn install(wrapper: ShellWrapper) {
    *slot().lock().unwrap_or_else(|e| e.into_inner()) = Some(wrapper);
}

/// 撤掉包装器（扩展被禁用时调用）。
pub fn clear() {
    *slot().lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// 取出当前包装器的一份句柄（锁外调用闭包）。未登记时为 `None`。
pub fn current() -> Option<ShellWrapper> {
    slot().lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// 当前是否登记了包装器（`/debug info` 一类状态展示用）。
pub fn is_installed() -> bool {
    slot().lock().unwrap_or_else(|e| e.into_inner()).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 装一个返回固定命令行的包装器；全局态，测试结束必须 [`clear`]。
    #[test]
    fn install_and_clear_roundtrip() {
        clear();
        assert!(!is_installed());
        assert!(current().is_none());

        install(Arc::new(|cwd: &Path| {
            Ok(SandboxedCommand {
                program: "bwrap".to_string(),
                prefix_args: vec!["--chdir".to_string(), cwd.display().to_string()],
            })
        }));
        assert!(is_installed());

        let wrapped = current().expect("wrapper")(Path::new("/tmp/work")).expect("ok");
        assert_eq!(wrapped.program, "bwrap");
        assert_eq!(wrapped.prefix_args[1], "/tmp/work");

        clear();
        assert!(!is_installed());
    }
}
