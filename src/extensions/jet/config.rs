//! `jet` 的配置层：`agent_dir()/extensions/jet.json`。
//!
//! 两个小节：`classifier`（规划复杂度分类器：`provider` + `model`）与
//! `router`（路由目标：`provider` + `planning` + `planningComplex` + `implementation`）。
//! 所有值都是字符串，**空串 = 未配置**——本扩展没有默认值，配置不完整就不注册虚拟模型。
//!
//! 与其它扩展一致：文件缺失 / 缺键 / 版本戳不符时自动回写缺省值，
//! 损坏或非对象文件不覆盖（读回缺省）。写入一律是"读-改-写"，保留其它键。

use super::super::util::str_field;
use crate::core::{config_cache::ConfigCache, settings_manager::agent_dir};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::RwLock,
};

/// 配置文件名（位于 `agent_dir()/extensions/` 下）
pub(super) const CONFIG_FILE: &str = "jet.json";

/// 配置目录子路径
const CONFIG_SUBDIR: &str = "extensions";

/// 配置版本戳
const CONFIG_VERSION: i64 = 1;

/// jet.json 读写锁（读配置与并发写盘）。
static CONFIG_LOCK: RwLock<()> = RwLock::new(());

/// jet.json 的缓存层（锁序恒为「RwLock → 缓存 Mutex」）。
static CONFIG_CACHE: ConfigCache = ConfigCache::new();

/// 分类器模型配置：`classifier` 小节；`provider` 与 `model` 都非空才算配置好。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct ClassifierConfig {
    /// 分类器模型所属 provider id（如 openrouter）；空串 = 未配置。
    pub provider: String,
    /// 分类器模型 id（如 `~typesafe/jev-latest`）；空串 = 未配置。
    pub model: String,
}

impl ClassifierConfig {
    /// 两项都非空才算配置完整；不完整时按"没有分类器"处理。
    pub(super) fn is_complete(&self) -> bool {
        !self.provider.trim().is_empty() && !self.model.trim().is_empty()
    }
}

/// 一个路由目标：provider + 模型 id；两项都非空才算配置好。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Target {
    /// 目标模型所属的 provider id；空串 = 未配置。
    pub provider: String,
    /// 目标模型 id；空串 = 未配置。
    pub model: String,
}

impl Target {
    /// provider 与模型 id 都非空。
    pub(super) fn is_complete(&self) -> bool {
        !self.provider.trim().is_empty() && !self.model.trim().is_empty()
    }

    /// 是否是某个物理模型（provider 与模型 id 都一致）。
    pub(super) fn matches(&self, provider: &str, model: &str) -> bool {
        self.provider == provider && self.model == model
    }
}

/// 路由目标配置：三档各自的 provider + 模型（**可以来自不同 provider**）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct RouterConfig {
    /// 规划阶段的目标（分类器判为普通、或未启用判定时使用）。
    pub planning: Target,
    /// 复杂任务的规划目标；未配置完整时分类器不参与判定。
    pub planning_complex: Target,
    /// 首次成功的 `edit`/`write` 之后接管整轮会话的目标。
    pub implementation: Target,
}

impl RouterConfig {
    /// 必填两档（`planning` / `implementation`）都配置完整。
    pub(super) fn is_complete(&self) -> bool {
        self.planning.is_complete() && self.implementation.is_complete()
    }

    /// 复杂规划目标；未配置完整时为 None（复杂度判定不参与）。
    pub(super) fn complex(&self) -> Option<&Target> {
        self.planning_complex
            .is_complete()
            .then_some(&self.planning_complex)
    }

    /// 三档目标及其命令行 flag 名（顺序 = `/jet` 状态与校验顺序）。
    pub(super) fn targets(&self) -> Vec<(&'static str, &Target)> {
        let mut out = vec![("--planning", &self.planning)];
        if let Some(complex) = self.complex() {
            out.push(("--planning-complex", complex));
        }
        out.push(("--implementation", &self.implementation));
        out
    }

    /// 三档目标及其命令行 flag 名，**不过滤未配置档**（顺序同上）。
    ///
    /// 仅用于需要成对比较的场合（如校验"本次改了哪档"）——列表长度恒为 3，不下标错位。
    pub(super) fn targets_all(&self) -> Vec<(&'static str, &Target)> {
        vec![
            ("--planning", &self.planning),
            ("--planning-complex", &self.planning_complex),
            ("--implementation", &self.implementation),
        ]
    }
}

/// jet.json 的完整配置（两个小节，缺省全空）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct JetConfig {
    /// 分类器小节。
    pub classifier: ClassifierConfig,
    /// 路由目标小节。
    pub router: RouterConfig,
}

/// 缺省 + 文件覆盖合并（纯函数，便于测试）。
///
/// 类型不符的键回落空串（不让一次手误把配置读成半截值）。
pub(super) fn merge_config(from_file: Option<&Value>) -> JetConfig {
    let classifier = from_file.and_then(|v| v.get("classifier"));
    let router = from_file.and_then(|v| v.get("router"));

    JetConfig {
        classifier: ClassifierConfig {
            provider: str_field(classifier, "provider"),
            model: str_field(classifier, "model"),
        },
        router: RouterConfig {
            planning: target_field(router.and_then(|r| r.get("planning"))),
            planning_complex: target_field(router.and_then(|r| r.get("planningComplex"))),
            implementation: target_field(router.and_then(|r| r.get("implementation"))),
        },
    }
}

/// 取一个目标小节（`{provider, model}`）；缺节 / 非对象 / 非字符串都回落空串。
fn target_field(value: Option<&Value>) -> Target {
    Target {
        provider: str_field(value, "provider"),
        model: str_field(value, "model"),
    }
}

/// 一个目标小节 → JSON（未配置的项写成空串，不删键）。
fn target_value(target: &Target) -> Value {
    json!({ "provider": target.provider, "model": target.model })
}

/// 配置 → JSON（camelCase 键；未配置的项写成空串，不删键）。
fn to_value(cfg: &JetConfig) -> Value {
    json!({
        "configVersion": CONFIG_VERSION,
        "classifier": {
            "provider": cfg.classifier.provider,
            "model": cfg.classifier.model,
        },
        "router": {
            "planning": target_value(&cfg.router.planning),
            "planningComplex": target_value(&cfg.router.planning_complex),
            "implementation": target_value(&cfg.router.implementation),
        },
    })
}

/// 文件是否缺键或版本戳不符（需要回写缺省值）。
fn needs_backfill(file: &Value) -> bool {
    /// 小节是否缺 `provider` / `model` 字符串键。
    fn missing_target(obj: Option<&Value>) -> bool {
        ["provider", "model"].iter().any(|k| {
            obj.and_then(|o| o.get(k))
                .and_then(|v| v.as_str())
                .is_none()
        })
    }

    let missing = match file.get("classifier") {
        None => true,
        Some(classifier) => missing_target(Some(classifier)),
    } || match file.get("router") {
        None => true,
        Some(router) => ["planning", "planningComplex", "implementation"]
            .iter()
            .any(|k| missing_target(router.get(k))),
    };

    missing || file.get("configVersion").and_then(|v| v.as_i64()) != Some(CONFIG_VERSION)
}

/// 配置文件路径：`agent_dir()/extensions/jet.json`
pub(super) fn config_path() -> PathBuf {
    agent_dir().join(CONFIG_SUBDIR).join(CONFIG_FILE)
}

/// 从磁盘加载配置；文件缺失 / 缺键 / 版本戳不符时回写（损坏文件不覆盖）。
pub(super) fn load() -> JetConfig {
    let path = config_path();
    let from_file = read_file_value(&path);
    let cfg = merge_config(from_file.as_ref());

    // 文件缺失且确实不存在 → 落一份缺省；存在但读不成对象（损坏）→ 不覆盖。
    let should_write = match &from_file {
        None => !path.exists(),
        Some(file) => needs_backfill(file),
    };

    if should_write {
        // 注意：不能在持有读锁时调用（`RwLock` 不可重入）。
        write_config(&path, &cfg);
    }
    cfg
}

/// 读配置文件的 JSON 对象（不存在/非法/非对象 → `None`）。走 [`CONFIG_CACHE`]。
fn read_file_value(path: &Path) -> Option<Value> {
    let _guard = CONFIG_LOCK.read().unwrap_or_else(|e| e.into_inner());
    let value = CONFIG_CACHE.read(path, || {
        fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .filter(|v| v.is_object())
            .unwrap_or(Value::Null)
    });
    (!value.is_null()).then_some(value)
}

/// 写盘（camelCase 键）。成功后回填 [`CONFIG_CACHE`]。
fn write_config(path: &Path, cfg: &JetConfig) {
    let _guard = CONFIG_LOCK.write().unwrap_or_else(|e| e.into_inner());

    if let Some(parent) = path.parent() {
        _ = fs::create_dir_all(parent);
    }

    let value = to_value(cfg);
    if let Ok(text) = serde_json::to_string_pretty(&value)
        && fs::write(path, format!("{text}\n")).is_ok()
    {
        CONFIG_CACHE.store(path, value);
    }
}

/// 读-改-写：`f` 在磁盘现值上施加改动，返回写回后的配置。
fn modify(f: impl FnOnce(&mut JetConfig)) -> JetConfig {
    let path = config_path();
    let mut cfg = merge_config(read_file_value(&path).as_ref());
    f(&mut cfg);
    write_config(&path, &cfg);
    cfg
}

/// 覆盖分类器小节（`/jet set <provider> <model-id>`），返回写回后的配置。
pub(super) fn set_classifier(provider: &str, model: &str) -> JetConfig {
    modify(|cfg| {
        cfg.classifier = ClassifierConfig {
            provider: provider.trim().to_string(),
            model: model.trim().to_string(),
        };
    })
}

/// 覆盖整个路由小节（增量合并由命令侧完成），返回写回后的配置。
pub(super) fn set_router(router: &RouterConfig) -> JetConfig {
    modify(|cfg| cfg.router = router.clone())
}

/// 清空全部配置（`/jet reset`），返回写回后的配置。
pub(super) fn reset() -> JetConfig {
    modify(|cfg| *cfg = JetConfig::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 取测试用临时 agent_dir 并加全局锁（配置读写走 agent_dir 单例）。
    fn setup() -> (
        crate::test_support::AgentDirGuard,
        std::sync::MutexGuard<'static, ()>,
    ) {
        let guard = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        (crate::test_support::AgentDirGuard::temp(), guard)
    }

    /// 一个目标小节（测试夹具）。
    fn target(provider: &str, model: &str) -> Target {
        Target {
            provider: provider.to_string(),
            model: model.to_string(),
        }
    }

    /// 三档各自一个 provider 的完整路由配置（测试夹具）。
    fn router_cfg() -> RouterConfig {
        RouterConfig {
            planning: target("openai", "gpt-5.6-terra"),
            planning_complex: target("anthropic", "claude-opus-4-8"),
            implementation: target("opencode", "kimi-k3"),
        }
    }

    #[test]
    fn merge_without_file_yields_empty_config() {
        let cfg = merge_config(None);
        assert_eq!(cfg, JetConfig::default());
        assert!(!cfg.router.is_complete());
        assert!(!cfg.classifier.is_complete());
        assert_eq!(cfg.router.complex(), None);
    }

    #[test]
    fn merge_reads_nested_sections_and_rejects_wrong_types() {
        let cfg = merge_config(Some(&json!({
            "classifier": { "provider": "openrouter", "model": "~typesafe/jev-latest" },
            "router": {
                "planning": { "provider": "openai", "model": "gpt-5.6-terra" },
                "planningComplex": { "provider": "anthropic", "model": "claude-opus-4-8" },
                "implementation": { "provider": "opencode", "model": "kimi-k3" },
            },
        })));
        assert_eq!(
            cfg.classifier,
            ClassifierConfig {
                provider: "openrouter".to_string(),
                model: "~typesafe/jev-latest".to_string(),
            }
        );
        assert!(cfg.router.is_complete());
        assert_eq!(cfg.router, router_cfg());
        assert_eq!(
            cfg.router.complex().map(|t| t.provider.as_str()),
            Some("anthropic"),
            "每档的 provider 各归各的"
        );

        // 非字符串 / 非对象回落空串
        let cfg = merge_config(Some(&json!({ "router": 42, "classifier": { "model": 7 } })));
        assert_eq!(cfg, JetConfig::default());

        let cfg = merge_config(Some(&json!({
            "router": { "planning": { "provider": "openai" } },
        })));
        assert!(!cfg.router.planning.is_complete(), "缺 model 视为未配置");
    }

    #[test]
    fn merge_treats_blank_strings_as_unset() {
        let cfg = merge_config(Some(&json!({
            "router": {
                "planning": { "provider": " ", "model": "a" },
                "implementation": { "provider": "openai", "model": "b" },
            },
        })));
        assert!(!cfg.router.is_complete(), "provider 只有空白 = 未配置");
        assert!(!cfg.router.planning.is_complete());
        assert!(cfg.router.implementation.is_complete());
        assert_eq!(
            cfg.router.targets().len(),
            2,
            "只有 planning 与 implementation"
        );
    }

    #[test]
    fn set_and_load_round_trip() {
        let (_ad, _g) = setup();

        set_classifier("openrouter", " ~typesafe/jev-latest ");
        let cfg = load();
        assert_eq!(cfg.classifier.provider, "openrouter");
        assert_eq!(cfg.classifier.model, "~typesafe/jev-latest");

        set_router(&router_cfg());
        assert_eq!(load().router, router_cfg());

        // 落盘保留分类器小节与版本戳
        let text = fs::read_to_string(config_path()).unwrap();
        assert!(text.contains("\"classifier\""), "got: {text}");
        assert!(text.contains("\"configVersion\""), "got: {text}");
        assert!(text.contains("\"planningComplex\""), "got: {text}");
    }

    #[test]
    fn targets_lists_configured_ones_in_order() {
        let mut router = router_cfg();
        assert_eq!(
            router
                .targets()
                .into_iter()
                .map(|(flag, _)| flag)
                .collect::<Vec<_>>(),
            vec!["--planning", "--planning-complex", "--implementation"]
        );

        // 复杂档缺失时不出现
        router.planning_complex = Target::default();
        assert_eq!(
            router
                .targets()
                .into_iter()
                .map(|(flag, _)| flag)
                .collect::<Vec<_>>(),
            vec!["--planning", "--implementation"]
        );
        assert_eq!(router.complex(), None);
    }

    #[test]
    fn reset_clears_both_sections_and_keeps_version() {
        let (_ad, _g) = setup();

        set_classifier("openrouter", "~typesafe/jev-latest");
        set_router(&router_cfg());

        assert_eq!(reset(), JetConfig::default());
        assert_eq!(load(), JetConfig::default());
        let text = fs::read_to_string(config_path()).unwrap();
        assert!(text.contains("\"configVersion\""), "got: {text}");
    }

    #[test]
    fn load_backfills_missing_keys() {
        let (_ad, _g) = setup();

        // 手写一份只有 router 的配置：加载后应补齐 classifier 与版本戳
        fs::create_dir_all(config_path().parent().unwrap()).unwrap();
        fs::write(
            config_path(),
            r#"{"router":{"planning":{"provider":"openai","model":"p"},"implementation":{"provider":"openai","model":"i"}}}"#,
        )
        .unwrap();

        let cfg = load();
        assert_eq!(cfg.router.planning.model, "p");
        let text = fs::read_to_string(config_path()).unwrap();
        assert!(text.contains("\"classifier\""), "got: {text}");
        assert!(text.contains("\"configVersion\""), "got: {text}");
    }

    #[test]
    fn corrupt_file_is_not_overwritten() {
        let (_ad, _g) = setup();

        fs::create_dir_all(config_path().parent().unwrap()).unwrap();
        fs::write(config_path(), "not json").unwrap();

        assert_eq!(load(), JetConfig::default());
        assert_eq!(fs::read_to_string(config_path()).unwrap(), "not json");
    }
}
