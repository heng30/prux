//! pr 的配置层：`agent_dir()/extensions/pr.json`。
//!
//! 一个文件、一组扁平键，没有项目级覆盖——**交付动作会写 git 与远端**，配置权留在用户
//! 自己的机器上比跟着仓库走更安全（提交一份 `.prux/extensions/pr.json` 就能改别人的
//! 推送目标，不值得）。
//!
//! | 键 | 缺省 | 含义 |
//! |----|------|------|
//! | `timeoutSecs` | `900` | `/pr run` 等 CI 出结论的总时限 |
//! | `pollSecs` | `20` | 轮询 CI 的间隔 |
//! | `base` | `""` | PR 的目标分支；空 = 用 `gh` 给的默认分支 |
//! | `draft` | `false` | 开 PR 时是否用 draft |
//!
//! 缺键回填、损坏文件不覆盖（与其它扩展同口径：用户要能看到自己写坏了什么）。
//!
//! 读路径走 [`ConfigCache`]：`/pr` 每次子命令都会问一次配置，而文件几乎不变。缓存以
//! `(path, mtime, len)` 判定有效性，手改文件仍即时生效。
//! 锁序恒为「[`CONFIG_LOCK`] → 缓存 `Mutex`」（见 `core::config_cache` 的模块文档）。

use crate::core::{config_cache::ConfigCache, settings_manager::agent_dir};
use serde_json::{Value, json};
use std::{path::PathBuf, sync::RwLock, time::Duration};

/// 扩展名（注册名，也是 `/extension` 面板里的名字）。
pub(super) const EXT: &str = "pr";
/// 斜杠命令名（`/pr`）。
pub(super) const CMD: &str = "pr";
/// 缺省等 CI 的总时限（秒）。
pub(super) const DEFAULT_TIMEOUT_SECS: u64 = 900;
/// 缺省轮询间隔（秒）。
pub(super) const DEFAULT_POLL_SECS: u64 = 20;

/// 全部键（用于判断缺键并回填）。
const KEYS: &[&str] = &["timeoutSecs", "pollSecs", "base", "draft"];

/// 配置文件读写锁（锁序恒为「RwLock → 缓存 Mutex」）。
static CONFIG_LOCK: RwLock<()> = RwLock::new(());

/// pr.json 的读缓存；只在 [`CONFIG_LOCK`] 作用域内读写。
static CONFIG_CACHE: ConfigCache = ConfigCache::new();

/// 一次交付的配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Config {
    /// `/pr run` 等 CI 的总时限。
    pub(super) timeout: Duration,
    /// 轮询 CI 的间隔。
    pub(super) poll: Duration,
    /// PR 目标分支；空串 = 由 `gh` 探测默认分支。
    pub(super) base: String,
    /// 开 PR 时是否标记 draft。
    pub(super) draft: bool,
}

/// 配置文件路径 `agent_dir()/extensions/pr.json`。
pub(super) fn config_path() -> PathBuf {
    agent_dir().join("extensions").join("pr.json")
}

/// 缺省 + 文件覆盖合并（纯函数，便于测试）。
pub(super) fn merge(from_file: Option<&Value>) -> Config {
    let get = |key: &str| from_file.and_then(|file| file.get(key));
    Config {
        timeout: Duration::from_secs(
            get("timeoutSecs")
                .and_then(Value::as_u64)
                .filter(|secs| *secs > 0)
                .unwrap_or(DEFAULT_TIMEOUT_SECS),
        ),
        poll: Duration::from_secs(
            get("pollSecs")
                .and_then(Value::as_u64)
                .filter(|secs| *secs > 0)
                .unwrap_or(DEFAULT_POLL_SECS),
        ),
        base: get("base")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default()
            .to_string(),
        draft: get("draft").and_then(Value::as_bool).unwrap_or(false),
    }
}

/// 把配置折回 JSON（缺省值也写全，用户改文件时能看见全部键）。
fn to_value(cfg: &Config) -> Value {
    json!({
        "timeoutSecs": cfg.timeout.as_secs(),
        "pollSecs": cfg.poll.as_secs(),
        "base": cfg.base,
        "draft": cfg.draft,
    })
}

/// 加载配置：缺键或文件不存在时回写；文件损坏时**不覆盖**。
pub(super) fn load() -> Config {
    let path = config_path();
    let from_file = CONFIG_CACHE.read_object(&CONFIG_LOCK, &path);
    let cfg = merge(from_file.as_ref());
    let needs_write = match &from_file {
        None => !path.exists(),
        Some(file) => KEYS.iter().any(|key| file.get(*key).is_none()),
    };

    if needs_write {
        _ = write(&cfg);
    }
    cfg
}

/// 写配置并回填缓存（落盘失败时不回填，不留「只在内存」的值）。
pub(super) fn write(cfg: &Config) -> std::io::Result<()> {
    CONFIG_CACHE.write_object(&CONFIG_LOCK, &config_path(), to_value(cfg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::AgentDirGuard;
    use std::fs;

    #[test]
    fn defaults_are_the_documented_ones() {
        let cfg = merge(None);
        assert_eq!(cfg.timeout, Duration::from_secs(DEFAULT_TIMEOUT_SECS));
        assert_eq!(cfg.poll, Duration::from_secs(DEFAULT_POLL_SECS));
        assert_eq!(cfg.base, "");
        assert!(!cfg.draft);
    }

    #[test]
    fn file_overrides_defaults_and_rejects_nonsense() {
        let cfg = merge(Some(&json!({
            "timeoutSecs": 0,
            "pollSecs": 3,
            "base": " develop ",
            "draft": true
        })));
        assert_eq!(
            cfg.timeout,
            Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            "0 秒回退缺省（否则等 CI 立刻超时）"
        );
        assert_eq!(cfg.poll, Duration::from_secs(3));
        assert_eq!(cfg.base, "develop");
        assert!(cfg.draft);

        let cfg = merge(Some(&json!({"unknownKey": 1})));
        assert_eq!(
            cfg.timeout,
            Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            "未知键被忽略"
        );
    }

    #[test]
    fn config_round_trips_and_backfills_missing_keys() {
        let _guard = AgentDirGuard::temp();
        let cfg = load();
        assert_eq!(cfg.poll, Duration::from_secs(DEFAULT_POLL_SECS));
        let path = config_path();
        assert!(path.exists(), "首次加载写回缺省");

        fs::write(&path, r#"{"pollSecs": 3}"#).unwrap();
        let cfg = load();
        assert_eq!(cfg.poll, Duration::from_secs(3), "已有值被保留");
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("timeoutSecs"), "缺键被回填: {text}");

        // 缓存按 (path, mtime, len) 失效：删掉文件后必须重建，不能拿内存里的旧值。
        fs::remove_file(&path).unwrap();
        let cfg = load();
        assert_eq!(cfg.poll, Duration::from_secs(DEFAULT_POLL_SECS));
        assert!(path.exists(), "文件被删除后重新写回");

        fs::write(&path, "{broken").unwrap();
        let cfg = load();
        assert_eq!(
            cfg.poll,
            Duration::from_secs(DEFAULT_POLL_SECS),
            "损坏文件退回缺省"
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "{broken",
            "损坏文件不覆盖"
        );
    }
}
