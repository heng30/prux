//! 把进程的 stderr 改道到日志文件。
//!
//! prux 是 TUI 程序：任何直接写 fd 2 的东西都会落在备用屏上，撕碎正在渲染的界面。
//! 外部命令能靠管道 / `Stdio::null()` 约束（见 [`crate::utils::process`]），但**进程内的
//! C 库约束不了**——`libxkbcommon` 解析合成器发来的 keymap 失败时就直接往 fd 2 打
//! `xkbcommon: ERROR: xkb_keymap_new_from_buffer: unsupported keymap format`，
//! `computer_*` 的输入注入每次建连都可能触发一次，几十行错误会糊满整屏。
//!
//! 所以进入 TUI 前用 [`StderrLog::install`] 把 fd 2 指向日志文件，`Drop` 时恢复：
//! 告警仍有去处，但不再进界面。少数本来就要用终端的场合（外部编辑器）用
//! [`with_terminal_stderr`] 临时换回真实 stderr。
//!
//! 改道依赖 `dup2`，只在 Unix 生效；其它平台是不改道的空实现（那里的输入注入不走 C 库）。

pub use imp::{StderrLog, uninstall, with_terminal_stderr};

#[cfg(unix)]
mod imp {
    use std::{
        fs::{self, File, OpenOptions},
        io,
        os::fd::OwnedFd,
        path::Path,
        sync::{Mutex, MutexGuard},
    };

    /// 日志文件的字节上限（1 MiB）：超过就在下次安装时清空重写，避免刷屏把文件撑爆。
    const MAX_LOG_BYTES: u64 = 1 << 20;

    /// 进程内唯一的改道状态；锁中毒不影响恢复，取回内部值继续用。
    static REDIRECT: Mutex<Option<Redirect>> = Mutex::new(None);

    /// 一次 stderr 改道；`Drop` 时把 fd 2 换回原样（退出路径上失败也无能为力，故静默）。
    pub struct StderrLog(());

    /// 安装期间保留的状态：原 stderr 的副本、日志文件句柄与当前是否改道中。
    struct Redirect {
        /// `dup` 出来的原 stderr 副本：恢复与临时换回都 dup2 它。
        original: OwnedFd,
        /// 日志文件：临时换回真实 stderr 后，再 dup2 它重新改道。
        log: File,
        /// `true` 表示 fd 2 当前指向日志文件；`with_terminal_stderr` 执行期间为 `false`。
        active: bool,
    }

    /// 取改道状态锁，忽略中毒标记（改道只是 fd 操作，没有需要守护的不变量）。
    fn lock() -> MutexGuard<'static, Option<Redirect>> {
        match REDIRECT.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    impl StderrLog {
        /// 把进程的 stderr 改道到 `path`（必要时创建父目录）。
        ///
        /// 已安装过时直接返回，不叠加第二次改道。父目录建不出来或文件打不开时返回错误，
        /// 调用方可忽略——那只是退回「C 库输出直写终端」，不影响功能。
        pub fn install(path: &Path) -> io::Result<Self> {
            let mut state = lock();
            if state.is_some() {
                return Ok(Self(()));
            }

            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let log = open_log(path)?;
            let original = nix::unistd::dup(io::stderr()).map_err(io::Error::other)?;
            nix::unistd::dup2_stderr(&log).map_err(io::Error::other)?;

            *state = Some(Redirect {
                original,
                log,
                active: true,
            });
            Ok(Self(()))
        }
    }

    impl Drop for StderrLog {
        /// 还回真实 stderr：此后（含退出时的错误输出）重新可见。
        fn drop(&mut self) {
            uninstall();
        }
    }

    /// 取消改道：fd 2 指回安装时 `dup` 出来的原 stderr。未安装时是空操作。
    pub fn uninstall() {
        let mut state = lock();
        if let Some(redirect) = state.take() {
            _ = nix::unistd::dup2_stderr(&redirect.original);
        }
    }

    /// 临时把 stderr 换回真实终端执行 `f`，返回后重新改道到日志（`f` panic 时也会恢复）。
    ///
    /// 只给「本来就要接管终端」的交互程序用（外部编辑器）：它们把提示与崩溃信息写 stderr，
    /// 送进日志文件用户就再也看不到了。未安装改道时直接执行 `f`。
    pub fn with_terminal_stderr<T>(f: impl FnOnce() -> T) -> T {
        let suspended = Suspended::new();
        let out = f();
        drop(suspended);
        out
    }

    /// [`with_terminal_stderr`] 的作用域守卫：`Drop` 时把 fd 2 重新指向日志文件。
    struct Suspended {
        /// `true` 表示进入时确实换回了真实 stderr，`Drop` 需要重新改道。
        restore: bool,
    }

    impl Suspended {
        /// 换回真实 stderr；未安装或已在真实 stderr 上时 `restore` 为 `false`。
        fn new() -> Self {
            let mut state = lock();
            let mut restore = false;
            if let Some(redirect) = state.as_mut()
                && redirect.active
                && nix::unistd::dup2_stderr(&redirect.original).is_ok()
            {
                redirect.active = false;
                restore = true;
            }
            Self { restore }
        }
    }

    impl Drop for Suspended {
        fn drop(&mut self) {
            if !self.restore {
                return;
            }

            let mut state = lock();
            if let Some(redirect) = state.as_mut()
                && !redirect.active
                && nix::unistd::dup2_stderr(&redirect.log).is_ok()
            {
                redirect.active = true;
            }
        }
    }

    /// 打开日志文件：追加写；已有内容超过 [`MAX_LOG_BYTES`] 时清空重写。
    fn open_log(path: &Path) -> io::Result<File> {
        let oversized = fs::metadata(path)
            .map(|meta| meta.len() > MAX_LOG_BYTES)
            .unwrap_or(false);
        OpenOptions::new()
            .create(true)
            .write(true)
            .append(!oversized)
            .truncate(oversized)
            .open(path)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::{io::Write, os::unix::fs::MetadataExt};

        /// fd 2 是进程全局的：涉及改道的测试必须串行，否则会吞掉并行测试的输出。
        static SERIAL: Mutex<()> = Mutex::new(());

        /// 当前 fd 2 指向的 inode。
        fn fd2_ino() -> u64 {
            use std::os::fd::BorrowedFd;
            // SAFETY: fd 2 在整个进程生命周期内都有效（启动时已打开，改道也是 dup2 到它）。
            let fd = unsafe { BorrowedFd::borrow_raw(nix::libc::STDERR_FILENO) };
            nix::sys::stat::fstat(fd).unwrap().st_ino
        }

        /// 改道后写 stderr 落到日志文件，`Drop` 后恢复真实 stderr。
        #[test]
        fn redirects_stderr_to_log_and_restores() {
            let _g = SERIAL.lock().unwrap();
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("logs").join("tui-stderr.log");

            let before = fd2_ino();
            {
                let _log = StderrLog::install(&path).unwrap();
                assert_eq!(fd2_ino(), fs::metadata(&path).unwrap().ino());
                writeln!(io::stderr(), "xkbcommon: ERROR: boom").unwrap();
            }
            assert_eq!(fd2_ino(), before);

            let logged = fs::read_to_string(&path).unwrap();
            assert!(
                logged.contains("xkbcommon: ERROR: boom"),
                "实际: {logged:?}"
            );
        }

        /// `with_terminal_stderr` 期间 fd 2 回到原目标，结束后重新指向日志。
        #[test]
        fn with_terminal_stderr_suspends_then_resumes() {
            let _g = SERIAL.lock().unwrap();
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("tui-stderr.log");

            let before = fd2_ino();
            let _log = StderrLog::install(&path).unwrap();
            let log_ino = fs::metadata(&path).unwrap().ino();
            assert_eq!(fd2_ino(), log_ino);

            with_terminal_stderr(|| assert_eq!(fd2_ino(), before));
            assert_eq!(fd2_ino(), log_ino);
        }

        /// 日志超过上限时下次安装清空重写，不无限增长。
        #[test]
        fn oversized_log_is_truncated_on_install() {
            let _g = SERIAL.lock().unwrap();
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("tui-stderr.log");
            fs::write(&path, vec![b'x'; (MAX_LOG_BYTES + 1) as usize]).unwrap();

            {
                let _log = StderrLog::install(&path).unwrap();
                writeln!(io::stderr(), "fresh").unwrap();
            }

            let logged = fs::read_to_string(&path).unwrap();
            assert_eq!(logged, "fresh\n");
        }
    }
}

#[cfg(not(unix))]
mod imp {
    use std::{io, path::Path};

    /// 非 Unix 平台的空实现：不碰 stderr，`Drop` 无事可做。
    pub struct StderrLog(());

    impl StderrLog {
        /// 恒返回一个不改道的句柄：这些平台的输入注入不走会写 fd 2 的 C 库。
        pub fn install(_path: &Path) -> io::Result<Self> {
            Ok(Self(()))
        }
    }

    /// 空操作：非 Unix 平台没有安装过改道。
    pub fn uninstall() {}

    /// 直接执行 `f`：非 Unix 平台没有改道需要临时取消。
    pub fn with_terminal_stderr<T>(f: impl FnOnce() -> T) -> T {
        f()
    }
}
