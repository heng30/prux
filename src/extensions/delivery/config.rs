//! delivery 的配置层：`agent_dir()/extensions/delivery.json`。
//!
//! 一个文件、一组扁平键，没有项目级覆盖——**交付动作会写 git 与远端**，配置权留在用户
//! 自己的机器上比跟着仓库走更安全（提交一份 `.prux/extensions/delivery.json` 就能改别人
//! 的推送目标，不值得）。
//!
//! | 键 | 缺省 | 含义 |
//! |----|------|------|
//! | `stage` | `"pr"` | 停在哪一步：`branch`（只提交）/ `push`（推到 origin）/ `pr`（开 PR 并跟 CI） |
//! | `maxIterations` | `2` | CI/评审不过时最多把失败喂回模型几次 |
//! | `timeoutSecs` | `900` | 每次结算等 CI 出结论的总时限 |
//! | `pollSecs` | `20` | 轮询 CI 的间隔 |
//! | `base` | `""` | PR 的目标分支；空 = 用 `gh` 给的默认分支 |
//! | `draft` | `false` | 开 PR 时是否用 draft |
//! | `requireEdits` | `true` | 只在本次回合调用过写类工具时才交付 |
//!
//! 缺键回填、损坏文件不覆盖（与其它扩展同口径：用户要能看到自己写坏了什么）。
//!
//! 读路径走 [`ConfigCache`]：`load()` 在每次结算都会调用，一个回合里可能问几十次，
//! 而配置文件几乎不变。缓存以 `(path, mtime, len)` 判定有效性，手改文件仍即时生效。
//! 锁序恒为「[`CONFIG_LOCK`] → 缓存 `Mutex`」（见 `core::config_cache` 的模块文档）。

use crate::{
    core::{config_cache::ConfigCache, settings_manager::agent_dir},
    utils::paths::write_json_file,
};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::RwLock,
    time::Duration,
};
use strum_macros::{EnumString, IntoStaticStr};

/// 扩展名（注册名，也是 `/extension` 面板里的名字）。
pub(super) const EXT: &str = "delivery";
/// 斜杠命令名（`/delivery`）。
pub(super) const CMD: &str = "delivery";
/// 缺省等 CI 的总时限（秒）。
pub(super) const DEFAULT_TIMEOUT_SECS: u64 = 900;
/// 缺省轮询间隔（秒）。
pub(super) const DEFAULT_POLL_SECS: u64 = 20;
/// 缺省喂回次数。
pub(super) const DEFAULT_MAX_ITERATIONS: usize = 2;

/// 全部键（用于判断缺键并回填）。
const KEYS: &[&str] = &[
    "stage",
    "maxIterations",
    "timeoutSecs",
    "pollSecs",
    "base",
    "draft",
    "requireEdits",
];

/// 配置文件读写锁（锁序恒为「RwLock → 缓存 Mutex」）。
static CONFIG_LOCK: RwLock<()> = RwLock::new(());

/// delivery.json 的读缓存；只在 [`CONFIG_LOCK`] 作用域内读写。
static CONFIG_CACHE: ConfigCache = ConfigCache::new();

/// 交付停在哪一步。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, EnumString, IntoStaticStr)]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
pub(super) enum Stage {
    /// 只把改动提交到本会话的分支，不碰远端。
    Branch,
    /// 提交并推到 `origin`，不开 PR。
    Push,
    /// 提交、推送、开 PR，并跟着 CI 与评审走（缺省）。
    #[default]
    Pr,
}

impl Stage {
    /// 配置与 `/delivery stage <x>` 接受的写法。
    pub(super) fn parse(raw: &str) -> Option<Stage> {
        raw.trim().to_ascii_lowercase().parse().ok()
    }

    /// 稳定的字符串标识（写配置与提示共用）。
    pub(super) fn as_label(self) -> &'static str {
        self.into()
    }
}

/// 一次交付的配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Config {
    /// 停在哪一步。
    pub(super) stage: Stage,
    /// CI 失败时最多喂回模型几次。
    pub(super) max_iterations: usize,
    /// 每次结算等 CI 的总时限。
    pub(super) timeout: Duration,
    /// 轮询 CI 的间隔。
    pub(super) poll: Duration,
    /// PR 目标分支；空串 = 由 `gh` 探测默认分支。
    pub(super) base: String,
    /// 开 PR 时是否标记 draft。
    pub(super) draft: bool,
    /// 只在本回合调用过写类工具时交付。
    pub(super) require_edits: bool,
}

/// 配置文件路径 `agent_dir()/extensions/delivery.json`。
pub(super) fn config_path() -> PathBuf {
    agent_dir().join("extensions").join("delivery.json")
}

/// 缺省 + 文件覆盖合并（纯函数，便于测试）。
pub(super) fn merge(from_file: Option<&Value>) -> Config {
    let get = |key: &str| from_file.and_then(|file| file.get(key));
    Config {
        stage: get("stage")
            .and_then(Value::as_str)
            .and_then(Stage::parse)
            .unwrap_or_default(),
        max_iterations: get("maxIterations")
            .and_then(Value::as_u64)
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_MAX_ITERATIONS),
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
        require_edits: get("requireEdits").and_then(Value::as_bool).unwrap_or(true),
    }
}

/// 把配置折回 JSON（缺省值也写全，用户改文件时能看见全部键）。
fn to_value(cfg: &Config) -> Value {
    json!({
        "stage": cfg.stage.as_label(),
        "maxIterations": cfg.max_iterations,
        "timeoutSecs": cfg.timeout.as_secs(),
        "pollSecs": cfg.poll.as_secs(),
        "base": cfg.base,
        "draft": cfg.draft,
        "requireEdits": cfg.require_edits,
    })
}

/// 读配置文件的 JSON 对象（不存在 / 非法 / 非对象都返回 `None`）。
fn read_file_value(path: &Path) -> Option<Value> {
    let _guard = CONFIG_LOCK.read().unwrap_or_else(|e| e.into_inner());
    let value = CONFIG_CACHE.read(path, || {
        fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .filter(Value::is_object)
            .unwrap_or(Value::Null)
    });
    (!value.is_null()).then_some(value)
}

/// 加载配置：缺键或文件不存在时回写；文件损坏时**不覆盖**。
pub(super) fn load() -> Config {
    let path = config_path();
    let from_file = read_file_value(&path);
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
    let path = config_path();
    let value = to_value(cfg);
    let _guard = CONFIG_LOCK.write().unwrap_or_else(|e| e.into_inner());
    let result = write_json_file(&path, &value);
    if result.is_ok() {
        CONFIG_CACHE.store(&path, value);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::AgentDirGuard;

    #[test]
    fn defaults_are_the_documented_ones() {
        let cfg = merge(None);
        assert_eq!(cfg.stage, Stage::Pr);
        assert_eq!(cfg.max_iterations, DEFAULT_MAX_ITERATIONS);
        assert_eq!(cfg.timeout, Duration::from_secs(DEFAULT_TIMEOUT_SECS));
        assert_eq!(cfg.poll, Duration::from_secs(DEFAULT_POLL_SECS));
        assert_eq!(cfg.base, "");
        assert!(!cfg.draft);
        assert!(cfg.require_edits);
    }

    #[test]
    fn file_overrides_defaults_and_rejects_nonsense() {
        let cfg = merge(Some(&json!({
            "stage": "PUSH",
            "maxIterations": 5,
            "timeoutSecs": 0,
            "pollSecs": 3,
            "base": " develop ",
            "draft": true,
            "requireEdits": false
        })));
        assert_eq!(cfg.stage, Stage::Push, "大小写不敏感");
        assert_eq!(cfg.max_iterations, 5);
        assert_eq!(
            cfg.timeout,
            Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            "0 秒回退缺省（否则等 CI 立刻超时）"
        );
        assert_eq!(cfg.poll, Duration::from_secs(3));
        assert_eq!(cfg.base, "develop");
        assert!(cfg.draft);
        assert!(!cfg.require_edits);

        let cfg = merge(Some(&json!({"stage": "merge"})));
        assert_eq!(cfg.stage, Stage::Pr, "未知阶段回退缺省");
    }

    #[test]
    fn config_round_trips_and_backfills_missing_keys() {
        let _guard = AgentDirGuard::temp();
        let cfg = load();
        assert_eq!(cfg.stage, Stage::Pr);
        let path = config_path();
        assert!(path.exists(), "首次加载写回缺省");

        fs::write(&path, r#"{"stage": "branch"}"#).unwrap();
        let cfg = load();
        assert_eq!(cfg.stage, Stage::Branch, "已有值被保留");
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("maxIterations"), "缺键被回填: {text}");

        // 缓存按 (path, mtime, len) 失效：删掉文件后必须重建，不能拿内存里的旧值。
        fs::remove_file(&path).unwrap();
        let cfg = load();
        assert_eq!(cfg.stage, Stage::Pr);
        assert!(path.exists(), "文件被删除后重新写回");

        fs::write(&path, "{broken").unwrap();
        let cfg = load();
        assert_eq!(cfg.stage, Stage::Pr, "损坏文件退回缺省");
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "{broken",
            "损坏文件不覆盖"
        );
    }
}
