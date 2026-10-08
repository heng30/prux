//! 测试支持接缝（仅测试构建编译：`cfg(test)` 或 `test-support` feature）。
//!
//! # 为什么存在
//!
//! 测试曾用 `std::env::set_var("PRUX_AGENT_DIR", ...)` + 进程级 `Mutex` 隔离，
//! 导致：环境变量/共享目录跨测试竞态、锁重入/跨 await 持锁死锁、跨进程同目录
//! 文件互踩。这里提供**线程本地**的 override（`cargo test` 每个测试一个线程，
//! 平行互不干扰，无需任何锁）与**超时护栏**（把挂起变成可诊断的 panic）。
//!
//! # 用法
//!
//! - 同步测试（绝大多数）：`let _g = test_support::AgentDirGuard::temp();`
//!   本线程的 `agent_dir()` 即指向一个全新临时目录，drop 时自动还原。
//!   同理 `test_support::HomeGuard` 覆盖 `HOME`（`~` 展开），
//!   `test_support::CwdGuard` 覆盖工作目录（项目层 `.prux/settings.json` 与信任判定）
//!   ——不要用 `set_current_dir`：那是进程级状态，会污染并行测试。
//! - 有 spawn 的异步测试（worker/agent 运行时在别的线程读 settings）：
//!   thread_local 传不过去，必须保留进程级 env 隔离见本模块 `pin_test_agent_dir`，
//!   由调用侧持单把叶级锁串行（无嵌套 → 无锁序死锁）。
//! - 网络/服务器类测试：`test_support::run_with_timeout(dur, || { ... })` 包住测试体。
//!   回调/闭包体内禁止再取任何锁（std `Mutex` 不可重入）。
#![cfg(any(test, feature = "test-support"))]

use std::{
    cell::RefCell,
    path::PathBuf,
    sync::{Mutex, MutexGuard, mpsc},
    time::Duration,
};
use tempfile::TempDir;

/// 搜索 provider 的凭据环境变量（`resolve_credential` 的兜底来源）。
pub const SEARCH_CREDENTIAL_ENV: [&str; 6] = [
    "TAVILY_API_KEY",
    "BRAVE_API_KEY",
    "EXA_API_KEY",
    "JINA_API_KEY",
    "SERPER_API_KEY",
    "PERPLEXITY_API_KEY",
];

std::thread_local! {
    /// 本线程的 agent 目录测试 override（无则回退默认目录）。
    static AGENT_DIR: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
    /// 本线程的 HOME 测试 override，供 `~` 展开使用。
    static HOME: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
    /// 本线程「交互式 settings UI 已接入」标志的测试 override。
    static SETTINGS_UI: RefCell<Option<bool>> = const { RefCell::new(None) };
    /// 本线程的工作目录测试 override（无则回退进程 cwd）。
    static CWD: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

/// `utils::paths::cwd()` 当前线程的测试 override（无则 None）。
pub fn cwd_override() -> Option<PathBuf> {
    CWD.with(|c| c.borrow().clone())
}

/// `agent_dir()` 当前线程的测试 override（无则 None）。
pub fn agent_dir_override() -> Option<PathBuf> {
    AGENT_DIR.with(|c| c.borrow().clone())
}

/// 交互式 settings UI 是否接入的当前线程 override（无则 None）。
pub fn settings_ui_override() -> Option<bool> {
    SETTINGS_UI.with(|c| *c.borrow())
}

/// `home_dir()` 当前线程的测试 override（无则 None）。
pub fn home_override() -> Option<PathBuf> {
    HOME.with(|c| c.borrow().clone())
}

/// RAII：测试期间把本线程 `PRUX_AGENT_DIR` 语义指向临时目录，drop 时还原。
///
/// 每实例一个**全新**临时目录 → 测试之间、进程之间零文件共享，隔离按构造成立。
pub struct AgentDirGuard {
    /// temp() 创建的临时目录，持有期间保证不被删除；set() 时为 None。
    _dir: Option<TempDir>,
    /// 替换前的本线程 override，drop 时还原；原本无 override 则为 None。
    prev: Option<PathBuf>,
}

impl AgentDirGuard {
    /// 指向全新临时目录（推荐：每测试独立，杜绝残留/竞态）。
    pub fn temp() -> Self {
        let dir = tempfile::tempdir().expect("tempdir for test agent dir");
        let path = dir.path().to_path_buf();
        let prev = AGENT_DIR.with(|c| c.borrow_mut().replace(path));
        AgentDirGuard {
            _dir: Some(dir),
            prev,
        }
    }

    /// 指向显式目录（调用方保证目录存活期 ≥ guard）。
    pub fn set(dir: impl Into<PathBuf>) -> Self {
        let prev = AGENT_DIR.with(|c| c.borrow_mut().replace(dir.into()));
        AgentDirGuard { _dir: None, prev }
    }
}

impl Drop for AgentDirGuard {
    /// 还原本线程的 agent 目录 override（原本无 override 则清空）。
    fn drop(&mut self) {
        AGENT_DIR.with(|c| *c.borrow_mut() = self.prev.take());
    }
}

/// RAII：测试期间覆盖本线程 `HOME`（`~` 展开/`expand_tilde` 用）。
pub struct HomeGuard {
    /// temp() 创建的临时目录，持有期间保证不被删除；set() 时为 None。
    _dir: Option<TempDir>,
    /// 替换前的本线程 override，drop 时还原；原本无 override 则为 None。
    prev: Option<PathBuf>,
}

impl HomeGuard {
    /// 指向一个全新临时 HOME 目录（目录随 guard 存活，drop 时随 guard 删除）。
    pub fn temp() -> Self {
        let dir = tempfile::tempdir().expect("tempdir for test home");
        let path = dir.path().to_path_buf();
        let prev = HOME.with(|c| c.borrow_mut().replace(path));
        HomeGuard {
            _dir: Some(dir),
            prev,
        }
    }

    /// 指向显式 HOME 目录（调用方保证目录存活期不短于 guard）。
    pub fn set(home: impl Into<PathBuf>) -> Self {
        let prev = HOME.with(|c| c.borrow_mut().replace(home.into()));
        HomeGuard { _dir: None, prev }
    }
}

impl Drop for HomeGuard {
    /// 还原本线程的 HOME override（原本无 override 则清空）。
    fn drop(&mut self) {
        HOME.with(|c| *c.borrow_mut() = self.prev.take());
    }
}

/// RAII：测试期间把本线程的工作目录指向 `path`，drop 时还原。
///
/// 用途与 [`HomeGuard`] 一致，但针对的是**项目层**配置（`.prux/settings.json`、项目信任）：
/// 那些读取走 `std::env::current_dir()`，而 `set_current_dir` 是**进程级**状态，
/// 并行测试下会让别的测试读到本测试的项目目录（或反之）。改用本 guard 后，
/// 「切目录」只对本线程可见，读项目层的测试也不再需要串行。
pub struct CwdGuard {
    /// 替换前的本线程 override，drop 时还原；原本无 override 则为 None。
    prev: Option<PathBuf>,
}

impl CwdGuard {
    /// 覆盖本线程 cwd（调用方保证目录存活期不短于 guard）。
    pub fn set(dir: impl Into<PathBuf>) -> Self {
        let prev = CWD.with(|c| c.borrow_mut().replace(dir.into()));
        CwdGuard { prev }
    }
}

impl Drop for CwdGuard {
    /// 还原本线程的 cwd override（原本无 override 则清空）。
    fn drop(&mut self) {
        CWD.with(|c| *c.borrow_mut() = self.prev.take());
    }
}

/// RAII：测试期间覆盖本线程「交互式 settings UI 已接入」标志，drop 时还原。
/// 生产用进程级 AtomicBool（UI 主线程设置、worker 线程读取）；测试并行运行，
/// 必须用线程本地 override 避免互踩。
pub struct SettingsUiGuard {
    /// 替换前的标志值，drop 时还原；原本无 override 则为 None。
    prev: Option<bool>,
}

impl SettingsUiGuard {
    /// 覆盖本线程「交互式 settings UI 已接入」标志为 `attached`；drop 时还原。
    pub fn set(attached: bool) -> Self {
        let prev = SETTINGS_UI.with(|c| c.borrow_mut().replace(attached));
        SettingsUiGuard { prev }
    }
}

impl Drop for SettingsUiGuard {
    /// 还原替换前的标志值（原本无 override 则清空）。
    fn drop(&mut self) {
        SETTINGS_UI.with(|c| *c.borrow_mut() = self.prev.take());
    }
}

/// 超时护栏：把测试体放到子线程执行，`dur` 内未完成则 panic（挂起 → 可诊断失败）。
///
/// 注意：超时后子线程会泄漏继续跑（进程随测试主线程退出而终止，无副作用累积）。
/// 子线程内 panic 会原样传回本线程重新 panic。
pub fn run_with_timeout<R, F>(dur: Duration, f: F) -> R
where
    R: Send + 'static,
    F: FnOnce() -> R + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        _ = tx.send(out);
    });
    match rx.recv_timeout(dur) {
        Ok(Ok(r)) => r,
        Ok(Err(p)) => std::panic::resume_unwind(p),
        Err(_) => panic!("test timed out after {dur:?}"),
    }
}

/// 叶级 env 键互斥锁。
///
/// `std::env` 没有线程本地版本：任何测试 set/remove 环境变量键（如
/// DEEPSEEK_API_KEY、PRUX_PROVIDER）都会影响同进程其他线程的读取。
/// 这些测试持单把叶级锁串行（无嵌套 → 无锁序死锁），agent_dir 隔离不受影响。
pub fn env_key_lock() -> MutexGuard<'static, ()> {
    /// 叶级环境变量互斥锁，串行化同进程内读写环境变量的测试。
    static ENV_KEY_LOCK: Mutex<()> = Mutex::new(());
    ENV_KEY_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// 测试期间清空所有搜索凭据环境变量（按 Drop 还原）。
///
/// 为什么需要：`Provider::is_available` 在配置未写密钥时会回退读进程环境变量，
/// 因此"未配置 provider"类断言在开发者机器上（已设 `TAVILY_API_KEY` 等）会失败。
/// 持 [`env_key_lock`] 串行，避免与其他 env 类测试互踩。
#[must_use = "guard 必须存活到断言结束"]
pub struct SearchEnvGuard {
    /// 持有的叶级 env 互斥锁，串行化同进程 env 读写直到 drop。
    _lock: MutexGuard<'static, ()>,
    /// 清空前的环境变量原值，drop 时逐个还原；None 表示原本未设置。
    saved: Vec<(&'static str, Option<String>)>,
}

impl Drop for SearchEnvGuard {
    /// 逐个还原保存的环境变量（原本未设置的重新移除），并释放持有的 env 锁。
    fn drop(&mut self) {
        for (name, value) in self.saved.drain(..) {
            match value {
                Some(value) => unsafe { std::env::set_var(name, value) },
                None => unsafe { std::env::remove_var(name) },
            }
        }
    }
}

/// 进入"无搜索凭据"环境（见 [`SearchEnvGuard`]）。
pub fn clear_search_credential_env() -> SearchEnvGuard {
    let lock = env_key_lock();
    let saved: Vec<(&'static str, Option<String>)> = SEARCH_CREDENTIAL_ENV
        .iter()
        .map(|name| (*name, std::env::var(name).ok()))
        .collect();
    for name in SEARCH_CREDENTIAL_ENV {
        unsafe { std::env::remove_var(name) };
    }
    SearchEnvGuard { _lock: lock, saved }
}

/// 测试串行化锁（叶级、无嵌套 → 无锁序死锁）：
/// - **env 类**（worker/sessions/commands/cli-auth 测试）：spawn 的 agent 在别的线程
///   读 settings/agent_dir（thread_local 传不过去），必须进程级 env pin + 串行；
/// - **注册表层**（全局 REGISTRY/EXTENSION_MODE 被测对象本身）：串行防互踩。
///
/// 同步测试不用它：用 `AgentDirGuard`（线程本地，无锁可并行）。
#[cfg(test)]
pub(crate) static AUTH_TEST_LOCK: Mutex<()> = Mutex::new(());

/// settings.json 覆写确认测试串行化锁：`PENDING_SETTINGS_OVERWRITE` 是进程级全局槽
/// （生产需跨 worker/UI 线程），并行测试会互踩，持本锁串行。
#[cfg(test)]
pub(crate) static SETTINGS_OVERWRITE_TEST_LOCK: Mutex<()> = Mutex::new(());

/// 测试用：把进程级 PRUX_AGENT_DIR 固定到共享测试目录（temp/prux-auth-tests），
/// 避免 handler 测试把 auth.json/settings.json 等写到真实 agent_dir。
/// 所有 test_agent/test_agent_cmd 等测试 helper 必须先调用本函数再碰磁盘。
/// 注意：若当前线程已有 AgentDirGuard 生效（同步测试的线程本地 override），
/// agent_dir() 会优先读线程本地，此处直接 no-op 返回，绝不写进程 env
/// （否则 env 类测试与 guard 类测试并行时会互踩进程级环境变量）。
#[cfg(test)]
pub(crate) fn pin_test_agent_dir() {
    if agent_dir_override().is_some() {
        return;
    }

    let dir = std::env::temp_dir().join("prux-auth-tests");
    _ = std::fs::create_dir_all(&dir);
    unsafe { std::env::set_var("PRUX_AGENT_DIR", &dir) };
}

/// 生成一张纯色 PNG 的 base64（不含 data URI 前缀）：内联图片与图片内容块测试用。
///
/// `width` / `height` 为像素尺寸，直接决定渲染时预留的单元格行列数。
pub fn png_base64(width: u32, height: u32) -> String {
    use base64::Engine as _;

    let img = image::RgbaImage::from_pixel(width, height, image::Rgba([10, 20, 30, 255]));
    let mut buf = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
        .expect("纯色 PNG 编码不应失败");

    base64::engine::general_purpose::STANDARD.encode(&buf)
}

/// 生成一张上下双色 PNG 的 base64：上半 `[10, 20, 30]`、下半 `[200, 100, 50]`。
///
/// 用于断言「终端真的画出了内容」——纯色图在 halfblocks 协议下每格上下同色，
/// 会被优化成空白字符（只带前景/背景色），双色图才会产生 `▀` / `▄` 字形。
///
/// 注意 base64 原文就是内联图片协议缓存的键：两个用例用同一尺寸取到的数据完全相同、
/// 共享同一条缓存，需要「首帧必为占位 / 首次请求必未命中」这类冷缓存前提的用例
/// 必须换个尺寸取图。
pub fn png_base64_two_tone(width: u32, height: u32) -> String {
    use base64::Engine as _;

    let mut img = image::RgbaImage::from_pixel(width, height, image::Rgba([10, 20, 30, 255]));
    for (_, y, px) in img.enumerate_pixels_mut() {
        if y >= height / 2 {
            *px = image::Rgba([200, 100, 50, 255]);
        }
    }

    let mut buf = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
        .expect("双色 PNG 编码不应失败");

    base64::engine::general_purpose::STANDARD.encode(&buf)
}
