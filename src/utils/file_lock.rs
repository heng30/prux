//! 跨进程文件锁（sidecar 锁文件）。
//!
//! 多个进程同时对同一份状态做「读-改-写」时，各自基于读到的旧快照回写，
//! 后写者会静默覆盖前写者刚写的键（lost update）。本模块把这类写串行到跨进程粒度：
//! 锁落在目标文件的 sidecar 上（`settings.json` → `settings.json.lock`），
//! 由 [`FileLock`] guard 持有，`Drop` 即释放。
//!
//! 两套实现，同一语义（独占、有界等待、持有者崩溃后可恢复）：
//!
//! - **unix**：`flock(LOCK_EX)`。锁随 fd 关闭 / 进程退出自动释放，崩溃不会留下
//!   需要人工清理的陈旧锁；sidecar 只被 flock、不参与目标文件的 rename 替换，
//!   故锁的 inode 稳定。
//! - **非 unix**：无 `flock`，用「token 写进同目录临时文件后 `hard_link` 到锁路径」
//!   制造原子出现的锁文件，拿不到锁时探测持有者 PID 是否存活来判定陈旧锁。
//!   锁文件必须**带内容原子出现**：若改用 `create_new` 再补写 token，会出现
//!   「文件已存在但内容为空」的窗口，等在门外的获取者会把空内容误判为陈旧锁并删掉
//!   持有者的锁，多个会话于是同时进入临界区。
//!
//! 陈旧锁的回收有两道判据：持有者 PID 已不在（unix 可探测），或锁文件已存在超过
//! [`STALE_LOCK_AGE`]。后者是**必需**的兜底：非 unix 探测不了 PID，只看 PID 的话
//! 崩溃遗留的锁文件会永远「看起来是活的」，把后来者全挡在门外（只能超时）。
//!
//! 等待是有界的（[`MAX_ATTEMPTS`] × [`RETRY_INTERVAL`]，约 1s）：写盘很快，拿不到锁
//! 说明另一个进程正在做同一段读-改-写；短暂退避后重试，超时返回
//! [`LockError::Timeout`]——宁可让调用方看到失败，也不无锁回写丢数据。

use std::{
    io,
    path::{Path, PathBuf},
    time::Duration,
};

/// 获取锁的重试上限（连同 [`RETRY_INTERVAL`] 即等待上限）。
pub const MAX_ATTEMPTS: u32 = 100;

/// 两次重试之间的退避间隔。
pub const RETRY_INTERVAL: Duration = Duration::from_millis(10);

/// 锁文件的陈旧判定阈值：持锁超过这个时长即视为崩溃遗留，可由后来者夺走。
///
/// 临界区只是一次「读-改-写」配置 / 任务文件（毫秒级），因此阈值给得很宽松：
/// 宁可多等一会儿，也不夺走一个活进程的锁。
pub const STALE_LOCK_AGE: Duration = Duration::from_secs(30);

/// 取锁失败的原因。调用方按自己的错误类型包装（`settings_manager` → `Error`，
/// `TaskStore` → `String`），本模块不依赖上层错误类型。
#[derive(Debug, thiserror::Error)]
pub enum LockError {
    /// 锁文件所在目录无法创建。
    #[error("failed to create lock dir {}: {source}", path.display())]
    CreateDir {
        /// 创建失败的目录路径。
        path: PathBuf,
        /// 底层 IO 错误原因。
        #[source]
        source: io::Error,
    },
    /// 锁文件无法打开。
    #[error("failed to open lock file {}: {source}", path.display())]
    Open {
        /// 无法打开的锁文件路径。
        path: PathBuf,
        /// 底层 IO 错误原因。
        #[source]
        source: io::Error,
    },
    /// 锁文件无法写入（非 unix 回退实现的临时文件）。
    #[error("failed to write lock file {}: {source}", path.display())]
    Write {
        /// 写入失败的临时锁文件路径。
        path: PathBuf,
        /// 底层 IO 错误原因。
        #[source]
        source: io::Error,
    },
    /// 取锁系统调用本身失败（非「被别人持有」）。
    #[error("failed to lock {}: {source}", path.display())]
    Lock {
        /// 加锁失败的锁文件路径。
        path: PathBuf,
        /// 取锁系统调用返回的错误。
        #[source]
        source: io::Error,
    },
    /// 有界等待耗尽：另一个进程一直在临界区里。
    #[error("timed out waiting for file lock {}", path.display())]
    Timeout { path: PathBuf },
}

/// 取锁的等待与陈旧判定参数。
#[derive(Debug, Clone, Copy)]
struct Tuning {
    /// 重试上限。
    max_attempts: u32,
    /// 两次重试之间的退避。
    retry: Duration,
    /// 锁文件的陈旧阈值（见 [`STALE_LOCK_AGE`]）。只有 `hard_link` 后端用得着
    /// （unix 走 flock，锁随进程退出自动释放，不需要年龄兜底）。
    #[cfg(any(not(unix), test))]
    stale_age: Duration,
}

impl Default for Tuning {
    /// 用模块级默认常量构造：重试上限、退避间隔与陈旧阈值。
    fn default() -> Self {
        Self {
            max_attempts: MAX_ATTEMPTS,
            retry: RETRY_INTERVAL,
            #[cfg(any(not(unix), test))]
            stale_age: STALE_LOCK_AGE,
        }
    }
}

/// 取锁的等待参数（默认值见 [`MAX_ATTEMPTS`] / [`RETRY_INTERVAL`] / [`STALE_LOCK_AGE`]）。
/// 供临界区不只是「读-改-写」的调用方覆盖，例如 MCP OAuth 刷新锁要等另一个进程的网络刷新。
#[derive(Debug, Clone, Copy)]
pub struct LockWait {
    /// 等待总预算（不足一次 [`Self::retry`] 时按一次尝试算）。
    pub total: Duration,
    /// 两次尝试之间的退避间隔（为 0 按 1ms 算，避免除零与忙等）。
    pub retry: Duration,
    /// 锁文件超过该时长视为崩溃遗留（仅非 unix 的 `hard_link` 后端使用）。
    pub stale_age: Duration,
}

/// 跨进程独占锁。`Drop` 即释放：unix 关掉 fd（`flock` 随之释放），
/// 非 unix 删除仍属于自己的锁文件。
#[derive(Debug)]
pub struct FileLock {
    /// unix 实现持有的 flock 句柄，Drop 时随文件关闭释放锁。
    #[cfg(unix)]
    _flock: nix::fcntl::Flock<std::fs::File>,
    /// 非 unix 回退实现持有的 hard_link 锁文件守卫。
    #[cfg(not(unix))]
    _link: link::LinkGuard,
}

impl FileLock {
    /// 获取 `target` 的跨进程独占锁（锁文件见 [`lock_path_for`]）。
    ///
    /// 锁文件所在目录不存在时先创建。拿不到锁时每 [`RETRY_INTERVAL`] 重试一次，
    /// 共 [`MAX_ATTEMPTS`] 次后返回 [`LockError::Timeout`]；期间的陈旧锁
    /// （持有者已不在，或已超过 [`STALE_LOCK_AGE`]）会被清掉后重试。
    pub fn acquire(target: &Path) -> Result<Self, LockError> {
        Self::acquire_with(target, Tuning::default())
    }

    /// 按 [`LockWait`] 取锁（阻塞等待），总预算换算成尝试次数。
    pub fn acquire_waiting(target: &Path, wait: LockWait) -> Result<Self, LockError> {
        let retry = wait.retry.max(Duration::from_millis(1));
        let attempts = (wait.total.as_millis() / retry.as_millis()).clamp(1, u128::from(u32::MAX));
        Self::acquire_with(
            target,
            Tuning {
                max_attempts: attempts as u32,
                retry,
                #[cfg(any(not(unix), test))]
                stale_age: wait.stale_age,
            },
        )
    }

    /// [`acquire`](Self::acquire) 的可调参数版本（测试用短超时）。
    fn acquire_with(target: &Path, tuning: Tuning) -> Result<Self, LockError> {
        let lock_path = lock_path_for(target);
        if let Some(parent) = lock_path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|source| LockError::CreateDir {
                path: parent.to_path_buf(),
                source,
            })?;
        }

        #[cfg(unix)]
        {
            Self::acquire_flock(&lock_path, tuning)
        }

        #[cfg(not(unix))]
        {
            Ok(Self {
                _link: link::acquire(&lock_path, tuning)?,
            })
        }
    }

    /// `flock(LOCK_EX)` 独占锁，非阻塞 + 退避重试。
    ///
    /// 同一个 fd 上反复重试：flock 的锁挂在 open file description 上，拿不到就说明
    /// 另一个进程（或本进程的另一处持锁点）正在临界区里。
    #[cfg(unix)]
    fn acquire_flock(lock_path: &Path, tuning: Tuning) -> Result<Self, LockError> {
        use nix::fcntl::{Flock, FlockArg};

        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(lock_path)
            .map_err(|source| LockError::Open {
                path: lock_path.to_path_buf(),
                source,
            })?;

        for attempt in 0..tuning.max_attempts {
            match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
                Ok(flock) => return Ok(Self { _flock: flock }),
                Err((returned, nix::errno::Errno::EWOULDBLOCK)) => {
                    file = returned;
                    if attempt + 1 < tuning.max_attempts {
                        std::thread::sleep(tuning.retry);
                    }
                }
                Err((_returned, e)) => {
                    return Err(LockError::Lock {
                        path: lock_path.to_path_buf(),
                        source: io::Error::other(e),
                    });
                }
            }
        }

        Err(LockError::Timeout {
            path: lock_path.to_path_buf(),
        })
    }
}

/// 非 unix 回退实现：`hard_link` 制造锁文件 + token 校验释放。
///
/// 非 unix 下无 `flock`，故自己实现独占与陈旧判定。测试构建下在 unix 也编译
/// （`cfg(test)`），以便这套逻辑在本机被真实执行到。
#[cfg(any(not(unix), test))]
mod link {
    use super::{LockError, Tuning};
    use std::{
        fs,
        io::ErrorKind,
        path::{Path, PathBuf},
        process,
        sync::atomic::{AtomicU64, Ordering},
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    /// 持有中的锁文件。`Drop` 时仅当文件里的 token 仍属于自己才删除。
    #[derive(Debug)]
    pub(super) struct LinkGuard {
        /// 锁文件路径。
        path: PathBuf,
        /// 写入锁文件的本进程令牌（`pid:id`），释放时用于确认归属。
        token: String,
    }

    impl Drop for LinkGuard {
        /// 释放锁文件，仅当 token 仍属于自己（见 [`release`]）。
        fn drop(&mut self) {
            release(&self.path, &self.token);
        }
    }

    /// 获取 `lock_path` 上的锁；拿不到就退避重试，共 `tuning.max_attempts` 次。
    pub(super) fn acquire(lock_path: &Path, tuning: Tuning) -> Result<LinkGuard, LockError> {
        let id = make_id();
        let token = format!("{}:{}", process::id(), id);

        // 同目录临时文件，保证 hard_link 不会跨文件系统。
        let tmp = lock_path.with_extension(format!("tmp.{id}"));
        fs::write(&tmp, token.as_bytes()).map_err(|source| LockError::Write {
            path: tmp.clone(),
            source,
        })?;

        // 取锁尝试与「清陈旧锁」各自计数：清掉陈旧锁是进展而非失败，不该吃掉重试预算
        // （否则 `max_attempts` 很小的调用方会因「清完就没预算再试」而失败）；但它也可能
        // 清不掉（目录只读、路径被占等），所以同样有界，不会死循环。
        let mut attempts = 0u32;
        let mut reclaims = 0u32;

        while attempts < tuning.max_attempts {
            match fs::hard_link(&tmp, lock_path) {
                Ok(()) => {
                    _ = fs::remove_file(&tmp);
                    return Ok(LinkGuard {
                        path: lock_path.to_path_buf(),
                        token,
                    });
                }
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                    if is_stale(lock_path, attempts, tuning) && reclaims < tuning.max_attempts {
                        reclaims += 1;
                        _ = fs::remove_file(lock_path);
                        continue;
                    }
                    attempts += 1;
                    if attempts < tuning.max_attempts {
                        std::thread::sleep(tuning.retry);
                    }
                }
                Err(source) => {
                    _ = fs::remove_file(&tmp);
                    return Err(LockError::Lock {
                        path: lock_path.to_path_buf(),
                        source,
                    });
                }
            }
        }

        _ = fs::remove_file(&tmp);
        Err(LockError::Timeout {
            path: lock_path.to_path_buf(),
        })
    }

    /// 锁文件是否是陈旧的（可以夺走）。
    ///
    /// token 形如 `<pid>:<id>`，两道判据取或：
    ///
    /// - **持有者已不在**：`is_process_running` 只在 unix 上真的在探测；
    /// - **持锁超过 `tuning.stale_age`**：这是非 unix 唯一的回收途径，不能省。
    ///   那里探测不了 PID（见 `is_process_running`），只看 PID 的话，崩溃遗留的锁文件
    ///   （PID 仍是个可解析的正数）会被永远当成活锁——后来者只能一遍遍超时，
    ///   再也写不进配置 / 任务文件。
    ///
    /// `attempt` 是已退避等待的轮数：内容为空 / 不可解析只可能来自崩溃或外部损坏，
    /// 也当陈旧，但要等两轮，避免误删一个极短命的写。
    fn is_stale(lock_path: &Path, attempt: u32, tuning: Tuning) -> bool {
        let content = fs::read_to_string(lock_path).unwrap_or_default();
        let pid = content
            .split(':')
            .next()
            .and_then(|s| s.parse::<i32>().ok())
            .unwrap_or(0);

        if pid > 0 {
            return !is_process_running(pid) || lock_file_older_than(lock_path, tuning.stale_age);
        }

        attempt >= 2
    }

    /// 锁文件存在时长是否已超过 `age`（读不到 mtime 时保守判否）。
    ///
    /// 锁文件一经 `hard_link` 出现就不再被触碰，故其 mtime 就是「取锁时刻」。
    fn lock_file_older_than(lock_path: &Path, age: Duration) -> bool {
        fs::metadata(lock_path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|created| SystemTime::now().duration_since(created).ok())
            .is_some_and(|held| held >= age)
    }

    /// 释放锁，但仅当我们仍持有它。
    ///
    /// 锁可能被活着的持有者之外的人夺走——`is_process_running` 从本机进程表作答，
    /// 因此另一个 PID namespace（容器，或经 NFS 共享的列表）的会话可能把我们的 PID
    /// 读成死的。没有 token 校验，我们就会删掉后继者的锁，两个会话同时写文件。
    pub(super) fn release(lock_path: &Path, token: &str) {
        if fs::read_to_string(lock_path).ok().as_deref() == Some(token) {
            _ = fs::remove_file(lock_path);
        }
    }

    /// 一次性 id：`{毫秒}-{纳秒 ^ pid}-{计数器}`，可安全进文件名。
    fn make_id() -> String {
        /// 进程内自增计数器，给同毫秒内生成的多个一次性 id 提供唯一序号。
        static SEQ: AtomicU64 = AtomicU64::new(0);

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        format!(
            "{}-{:x}-{:x}",
            now.as_millis(),
            (now.subsec_nanos() as u64) ^ u64::from(process::id()),
            SEQ.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// 用 `kill(pid, 0)` 探测进程是否存活（无权限时报错，视作存活）。
    #[cfg(unix)]
    fn is_process_running(pid: i32) -> bool {
        use nix::{sys::signal::kill, unistd::Pid};
        kill(Pid::from_raw(pid), None).is_ok()
    }

    /// 非 unix 无法探测进程存活，一律返回 `true`（陈旧回收只能靠锁文件年龄）。
    #[cfg(not(unix))]
    fn is_process_running(_pid: i32) -> bool {
        // 探测不了存活，一律当活着：陈旧判定完全交给锁文件年龄
        // （[`super::STALE_LOCK_AGE`]），否则崩溃遗留的锁文件永远收不回来。
        true
    }
}

/// 目标文件对应的 sidecar 锁文件路径：`foo.json` → `foo.json.lock`。
///
/// 与目标文件同目录，保证临时文件 / `hard_link` 不跨文件系统。
pub fn lock_path_for(target: &Path) -> PathBuf {
    let mut s = target.as_os_str().to_os_string();
    s.push(".lock");
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_target() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("state.json");
        (dir, target)
    }

    /// 快节奏测试参数：1ms 退避、3 次重试。
    fn fast_tuning() -> Tuning {
        Tuning {
            max_attempts: 3,
            retry: Duration::from_millis(1),
            ..Tuning::default()
        }
    }

    /// 同上，但锁文件永不因年龄而陈旧。
    fn fast_tuning_never_stale() -> Tuning {
        Tuning {
            stale_age: Duration::from_secs(3600),
            ..fast_tuning()
        }
    }

    #[test]
    fn lock_path_is_sibling() {
        assert_eq!(
            lock_path_for(Path::new("/tmp/a/settings.json")),
            PathBuf::from("/tmp/a/settings.json.lock")
        );
    }

    #[test]
    fn held_lock_blocks_second_acquire_and_releases_on_drop() {
        let (_dir, target) = temp_target();

        let held = FileLock::acquire(&target).unwrap();
        let err = FileLock::acquire_with(&target, fast_tuning_never_stale()).unwrap_err();
        assert!(matches!(err, LockError::Timeout { .. }), "got {err:?}");

        drop(held);
        // 释放后可再取（unix 的锁文件会留下，非 unix 的会删掉，两者都不影响再取）。
        FileLock::acquire(&target).unwrap();
    }

    #[test]
    fn serializes_read_modify_write_across_threads() {
        let (_dir, target) = temp_target();
        std::fs::write(&target, "0").unwrap();

        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| {
                    let _lock = FileLock::acquire(&target).unwrap();
                    let n: u64 = std::fs::read_to_string(&target)
                        .unwrap()
                        .trim()
                        .parse()
                        .unwrap();
                    std::thread::sleep(Duration::from_millis(1));
                    std::fs::write(&target, (n + 1).to_string()).unwrap();
                });
            }
        });

        assert_eq!(std::fs::read_to_string(&target).unwrap(), "8");
    }

    #[test]
    fn missing_parent_dir_is_created() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("nested/deeper/state.json");

        FileLock::acquire(&target).unwrap();
        assert!(lock_path_for(&target).exists());
    }

    // ── 非 unix 回退实现（unix 测试构建下也编译，故可在此直接跑） ──

    #[test]
    fn link_backend_is_exclusive_and_releases() {
        let (_dir, target) = temp_target();
        let lock_path = lock_path_for(&target);
        let tuning = fast_tuning_never_stale();

        let held = link::acquire(&lock_path, tuning).unwrap();
        let err = link::acquire(&lock_path, tuning).unwrap_err();
        assert!(matches!(err, LockError::Timeout { .. }), "got {err:?}");

        drop(held);
        assert!(!lock_path.exists(), "释放应删掉自己的锁文件");
        link::acquire(&lock_path, tuning).unwrap();
    }

    #[test]
    fn link_backend_reclaims_stale_lock_of_dead_process() {
        let (_dir, target) = temp_target();
        let lock_path = lock_path_for(&target);
        std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        let tuning = fast_tuning_never_stale();

        // 持有者 PID 远超 pid_max：视作已死，锁可被夺走。
        std::fs::write(&lock_path, format!("{}:dead", i32::MAX)).unwrap();
        let held = link::acquire(&lock_path, tuning).unwrap();

        // 夺锁后旧 token 的持有者不能再删我们的锁。
        link::release(&lock_path, &format!("{}:dead", i32::MAX));
        assert!(lock_path.exists(), "旧持有者不得删掉后继者的锁");
        drop(held);
    }

    /// 非 unix 无法探测 PID，只能靠年龄回收：否则崩溃遗留的锁文件会把后来者
    /// 永远挡在门外（每回都超时）。这里用「自己（活着的 PID）+ 已超龄」模拟。
    #[test]
    fn link_backend_reclaims_lock_held_longer_than_stale_age() {
        let (_dir, target) = temp_target();
        let lock_path = lock_path_for(&target);
        std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        std::fs::write(&lock_path, format!("{}:held", std::process::id())).unwrap();

        // 未超龄：持有者（本进程）确实活着，不得夺锁。
        let err = link::acquire(&lock_path, fast_tuning_never_stale()).unwrap_err();
        assert!(matches!(err, LockError::Timeout { .. }), "got {err:?}");

        // 已超龄：当崩溃遗留处理。
        let expired = Tuning {
            stale_age: Duration::ZERO,
            ..fast_tuning()
        };
        link::acquire(&lock_path, expired).unwrap();
    }

    #[test]
    fn link_backend_reclaims_empty_lock_file_after_waiting() {
        let (_dir, target) = temp_target();
        let lock_path = lock_path_for(&target);
        std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();

        // 空 / 不可解析只可能来自崩溃或外部损坏：等两轮后当陈旧。
        std::fs::write(&lock_path, "").unwrap();
        link::acquire(&lock_path, fast_tuning_never_stale()).unwrap();
    }
}
