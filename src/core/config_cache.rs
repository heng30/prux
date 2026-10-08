//! 配置文件读缓存
//!
//! 每个配置文件一个 [`ConfigCache`]（内部是 `OnceLock<Mutex<Option<Cached>>>`，
//! `const` 构造，可直接用作 `static`）：
//! - 读：[`ConfigCache::read`] 在 `(path, mtime, len)` 与缓存项一致时直接返回内存值，
//!   否则调用传入的读取闭包读盘并回填；
//! - 写：落盘成功后 [`ConfigCache::store`] 取写盘后的元数据回填，下次读取即命中。
//!
//! 有效性以 `(path, mtime, len)` 判定：文件被手工编辑、被其它进程改写、被删除，或换了
//! agent_dir / 项目目录，都会 miss 而重新读盘，外部改动仍能即时生效；命中也避免了重复的
//! 读盘 + JSON 解析。
//!
//! **锁序**：本类型只持有内层 `Mutex`。调用方必须已持该配置文件的 `RwLock`（读锁或写锁均可），
//! 使锁序恒为「RwLock → 缓存 `Mutex`」。绝不可在持有缓存 `Mutex` 时再去取 `RwLock`，否则可能自死锁。

use crate::utils::paths::file_stamp;
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::SystemTime,
};

/// 单个配置文件的缓存项。
struct Cached {
    /// 缓存对应的文件路径，换路径即视为未命中。
    path: PathBuf,
    /// 读盘时的修改时间；None = 元数据不可得（文件不存在等）。
    mtime: Option<SystemTime>,
    /// 读盘时的字节长度；None = 元数据不可得。
    len: Option<u64>,
    /// 上次成功读到的 JSON 内容（不缓存中间态）。
    value: Value,
}

/// 一个配置文件的缓存层。
pub(crate) struct ConfigCache {
    /// 惰性初始化的缓存槽；None = 尚未读盘缓存过。
    inner: OnceLock<Mutex<Option<Cached>>>,
}

impl ConfigCache {
    /// `const` 构造：`static CACHE: ConfigCache = ConfigCache::new();`
    pub(crate) const fn new() -> Self {
        Self {
            inner: OnceLock::new(),
        }
    }

    /// 惰性初始化并返回缓存槽的 `Mutex` 引用（首次调用时创建）。
    fn slot(&self) -> &Mutex<Option<Cached>> {
        self.inner.get_or_init(|| Mutex::new(None))
    }

    /// 读缓存：`(path, mtime, len)` 全部吻合则命中返回，否则调用 `read` 读盘并回填。
    ///
    /// **调用方必须已持该配置文件的 `RwLock`**（见模块文档的锁序说明）。
    ///
    /// `read` 是 `Fn` 而非 `FnOnce`：读取期间若检测到文件元数据变化（并发非原子写 /
    /// 编辑器保存 / 崩溃恢复留下的截断态），会重读而非把中间态（尤其空对象）返回给
    /// 调用方——一次空读被后续 read-modify-write 固化就会丢掉 `disabledExtensions`
    /// 等键。连续多次仍不稳定时退回上次成功快照，绝不缓存中间态。
    pub(crate) fn read(&self, path: &Path, read: impl Fn() -> Value) -> Value {
        let mut guard = self.slot().lock().unwrap_or_else(|e| e.into_inner());
        let (mut mtime, mut len) = file_stamp(path);
        if let Some(cached) = guard.as_ref()
            && cached.path.as_path() == path
            && cached.mtime == mtime
            && cached.len == len
        {
            return cached.value.clone();
        }

        // 读取前后元数据一致才算“稳定”。不一致说明读到了写入中间态。
        /// 判定读取到的文件快照稳定前允许的最大重试次数。
        const MAX_ATTEMPTS: usize = 4;
        let mut stable: Option<Value> = None;
        let mut last: Option<Value> = None;
        for attempt in 0..MAX_ATTEMPTS {
            let value = read();
            let (after_mtime, after_len) = file_stamp(path);
            let unchanged = after_mtime == mtime && after_len == len;

            mtime = after_mtime;
            len = after_len;
            last = Some(value);

            if unchanged {
                stable = last.take();
                break;
            }

            if attempt + 1 < MAX_ATTEMPTS {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        }

        // 极端情况：连续多次都检测到文件在变化（持续高频写）。
        // 不缓存中间态，退回上次成功读取的快照；无快照才用最后一次读取结果。
        let (value, cacheable) = match stable {
            Some(v) => (v, true),
            None => (
                guard
                    .as_ref()
                    .filter(|c| c.path.as_path() == path)
                    .map(|c| c.value.clone())
                    .or(last)
                    .unwrap_or(Value::Null),
                false,
            ),
        };

        if cacheable {
            *guard = Some(Cached {
                path: path.to_path_buf(),
                mtime,
                len,
                value: value.clone(),
            });
        }
        value
    }

    /// 主动失效：丢弃缓存项，下次读取重新走磁盘。
    ///
    /// 一般无需调用：外部改写/删除文件由 `(path, mtime, len)` 自动失效。仅用于
    /// 「要求强制重读」的场景（如扩展被启用时希望忽略上一次的内存快照）。
    /// 调用方同样必须已持该配置文件的 `RwLock`。
    pub(crate) fn invalidate(&self) {
        *self.slot().lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// 写盘成功后回填缓存（取写盘后的元数据）。
    ///
    /// 落盘失败时不要调用，避免缓存持有「只在内存、未写盘」的值。
    /// 调用方已持写锁，读者拿不到读锁，看不到中间态。
    pub(crate) fn store(&self, path: &Path, value: Value) {
        let (mtime, len) = file_stamp(path);
        *self.slot().lock().unwrap_or_else(|e| e.into_inner()) = Some(Cached {
            path: path.to_path_buf(),
            mtime,
            len,
            value,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn read_json(path: &Path) -> Value {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_else(|| json!({}))
    }

    /// 回归：读取期间文件被（非原子地）改写，读到的截断中间态绝不能返回给调用方。
    ///
    /// 背景：一次「空读」若被后续 read-modify-write 当成基准回写，就会把
    /// `disabledExtensions` 等键永久洗掉。旧实现（`cached_settings`）有重试，
    /// `04e949a` 重构换成 `ConfigCache` 后丢掉了这层保护。
    #[test]
    fn mid_write_value_is_retried_and_not_returned() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cached.json");
        std::fs::write(&path, r#"{"k":"old","pad":"xxxx"}"#).unwrap();

        let cache = ConfigCache::new();
        let calls = std::cell::Cell::new(0u32);
        let value = cache.read(&path, || {
            let n = calls.get();
            calls.set(n + 1);
            match n {
                // 第一次：模拟并发的 truncate 中间态（读到空）
                0 => {
                    std::fs::write(&path, "").unwrap();
                    json!({})
                }
                // 第二次：写完成，读完整内容
                1 => {
                    std::fs::write(&path, r#"{"k":"new","pad":"yyyy"}"#).unwrap();
                    read_json(&path)
                }
                // 之后：文件稳定
                _ => read_json(&path),
            }
        });

        assert_eq!(
            value.get("k").and_then(|v| v.as_str()),
            Some("new"),
            "不得把写入中间态返回给调用方: {value}"
        );
        assert!(calls.get() >= 2, "检测到读取期间变化后必须重读");
    }

    /// 元数据稳定时正常缓存：第二次读取不应再调用读取闭包。
    #[test]
    fn stable_read_is_cached() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cached.json");
        std::fs::write(&path, r#"{"k":1}"#).unwrap();

        let cache = ConfigCache::new();
        let calls = std::cell::Cell::new(0u32);
        let first = cache.read(&path, || {
            calls.set(calls.get() + 1);
            read_json(&path)
        });
        let second = cache.read(&path, || {
            calls.set(calls.get() + 1);
            read_json(&path)
        });

        assert_eq!(first, second);
        assert_eq!(calls.get(), 1, "稳定文件第二次读取应命中缓存");
    }
}
