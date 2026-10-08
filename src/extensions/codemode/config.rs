//! `codemode` 的配置层：`agent_dir()/extensions/codemode.json`。
//!
//! 两个键（都在 `/extension` 面板里改，见 [`panel_settings`]）：
//! - `mode`：`on`（默认）/ `only`，脚本可调的工具在模型侧怎么呈现，语义见
//!   [`super::description::Mode`]；
//! - `inlineBudget`：`codemode` 描述里工具声明可用的估算 token 预算（字符数 ÷ 4），
//!   缺省 [`super::description::DEFAULT_INLINE_BUDGET`]；`0` = 只列命名空间。
//!
//! 与其它扩展一致：文件缺失 / 升级后缺键 / 版本戳不符时自动回写缺省值，
//! 损坏或非对象文件不覆盖（读回缺省）。

use super::description::{self, Mode};
use crate::{
    core::{config_cache::ConfigCache, extensions::ExtensionSetting, settings_manager::agent_dir},
    extensions::util::{choice_num, cycle_setting},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::RwLock,
};
use strum::IntoEnumIterator;

/// 配置文件名（位于 `agent_dir()/extensions/` 下）
pub(super) const CONFIG_FILE: &str = "codemode.json";

/// 配置目录子路径
const CONFIG_SUBDIR: &str = "extensions";

/// 配置版本戳
const CONFIG_VERSION: i64 = 1;

/// 字符串缺省键
pub(super) const STRING_DEFAULTS: &[(&str, &str)] = &[("mode", "on")];

/// 数值缺省键（token 数）
pub(super) const NUMERIC_DEFAULTS: &[(&str, f64)] =
    &[("inlineBudget", description::DEFAULT_INLINE_BUDGET as f64)];

/// `inlineBudget` 面板档位：`(token 数, 面板标签)`
const BUDGET_TABLE: &[(f64, &str)] = &[
    (0.0, "none"),
    (1_500.0, "small"),
    (3_000.0, "normal"),
    (6_000.0, "large"),
    (12_000.0, "huge"),
];

/// codemode.json 读写锁（面板应用设置与读配置可能并发）
static CONFIG_LOCK: RwLock<()> = RwLock::new(());

/// codemode.json 的缓存层（锁序恒为「RwLock → 缓存 Mutex」）
static CONFIG_CACHE: ConfigCache = ConfigCache::new();

/// 配置文件路径：`agent_dir()/extensions/codemode.json`
pub(super) fn config_path() -> PathBuf {
    agent_dir().join(CONFIG_SUBDIR).join(CONFIG_FILE)
}

/// `mode` 面板档位（[`Mode`] 的规范名，顺序即面板里的循环顺序）
fn mode_choices() -> Vec<&'static str> {
    Mode::iter().map(Mode::as_str).collect()
}

/// 运行期配置：键 → JSON 值，缺省齐全
#[derive(Clone)]
pub(super) struct Config {
    /// 扁平配置表：`mode`（字符串）与 `inlineBudget`（数值），由缺省值与文件覆盖合并而来
    values: BTreeMap<String, Value>,
}

impl Config {
    /// `mode` 的解析结果（未知取值回落 [`Mode::default`]，即 `On`）
    pub(super) fn mode(&self) -> Mode {
        self.values
            .get("mode")
            .and_then(|v| v.as_str())
            .and_then(Mode::parse)
            .unwrap_or_default()
    }

    /// `mode` 的字符串形式（与 [`Config::mode`] 的解析结果一致）
    pub(super) fn mode_label(&self) -> &'static str {
        self.mode().as_str()
    }

    /// `inlineBudget`：非负数按整数取，缺键/类型不符回落缺省预算
    pub(super) fn inline_budget(&self) -> usize {
        self.values
            .get("inlineBudget")
            .and_then(|v| v.as_f64())
            .filter(|v| *v >= 0.0)
            .map(|v| v as usize)
            .unwrap_or(description::DEFAULT_INLINE_BUDGET)
    }

    /// 按字符串读取（缺键或类型不符时回退空串）
    fn text(&self, key: &str) -> String {
        self.values
            .get(key)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    }
}

/// 全部缺省键（含 `configVersion` 之外的键；用于回填判断）
pub(super) fn default_keys() -> Vec<String> {
    let mut keys: Vec<String> = STRING_DEFAULTS.iter().map(|(k, _)| k.to_string()).collect();
    keys.extend(NUMERIC_DEFAULTS.iter().map(|(k, _)| k.to_string()));
    keys
}

/// 缺省 + 文件覆盖合并（纯函数，便于测试）。
///
/// 类型不符的键回落缺省（不让一次手误把配置读成 0 / 空）。
pub(super) fn merge_config(from_file: Option<&Value>) -> Config {
    let mut values: BTreeMap<String, Value> = BTreeMap::new();

    for (key, default) in STRING_DEFAULTS {
        let value = from_file
            .and_then(|f| f.get(key))
            .filter(|v| v.is_string())
            .cloned()
            .unwrap_or_else(|| json!(default));
        values.insert((*key).to_string(), value);
    }
    for (key, default) in NUMERIC_DEFAULTS {
        let value = from_file
            .and_then(|f| f.get(key))
            .and_then(|v| v.as_f64())
            .filter(|v| v.is_finite() && *v >= 0.0 && v.fract() == 0.0)
            .map(|v| json!(v as i64))
            .unwrap_or_else(|| json!(*default as i64));
        values.insert((*key).to_string(), value);
    }

    values.insert("configVersion".to_string(), json!(CONFIG_VERSION));
    Config { values }
}

/// 从磁盘加载配置；文件缺失 / 缺键 / 版本戳不符时回写（损坏文件不覆盖）
pub(super) fn load_config() -> Config {
    let path = config_path();
    let from_file = read_file_value(&path);
    let cfg = merge_config(from_file.as_ref());

    let should_write = match &from_file {
        None => !path.exists(),
        Some(file) => {
            default_keys().iter().any(|k| file.get(k).is_none())
                || file.get("configVersion").and_then(|v| v.as_i64()) != Some(CONFIG_VERSION)
        }
    };

    if should_write {
        // 注意：不能在持有读锁时调用（`RwLock` 不可重入）。
        write_config(&path, &cfg);
    }
    cfg
}

/// `/extension` 面板里 `codemode` 的设置项：`mode` 与 `inlineBudget`。
pub(super) fn panel_settings() -> Vec<ExtensionSetting> {
    let cfg = load_config();
    vec![
        cycle_setting(
            "mode",
            "Declaration mode",
            "How script-callable tools are presented to the model: `on` declares direct tools as usual (each description gains a script sample), `only` moves them all into the codemode description so the model must call them from a script",
            cfg.text("mode"),
            &mode_choices(),
        ),
        cycle_setting(
            "inlineBudget",
            "Inline budget",
            "Estimated token budget (chars ÷ 4) for inlining tool declarations into the codemode description; tools that do not fit are only counted by namespace and can be found with searchTools()/describeTool()/describeNamespace()",
            budget_label(&cfg),
            &["none", "small", "normal", "large", "huge"],
        ),
    ]
}

/// 当前 `inlineBudget` 对应的面板标签（不在档位表里时回退到原始数字）
fn budget_label(cfg: &Config) -> String {
    let current = cfg.inline_budget() as f64;
    BUDGET_TABLE
        .iter()
        .find(|(v, _)| (*v - current).abs() < f64::EPSILON)
        .map(|(_, l)| (*l).to_string())
        .unwrap_or_else(|| current.to_string())
}

/// 应用面板选择并落盘（面板是「设置」语义）；非法键 / 非法取值返回 `Err`。
pub(super) fn apply_panel_choice(key: &str, value: &str) -> std::result::Result<Config, String> {
    match key {
        "mode" => {
            if !mode_choices().contains(&value) {
                return Err(format!("invalid option: {value}"));
            }
            Ok(modify(|cfg| {
                cfg.values.insert("mode".to_string(), json!(value));
            }))
        }
        "inlineBudget" => {
            let budget = choice_num(BUDGET_TABLE, value)?;
            Ok(modify(|cfg| {
                cfg.values
                    .insert("inlineBudget".to_string(), json!(budget as i64));
            }))
        }
        other => Err(format!("unknown setting: {other}")),
    }
}

/// 读-改-写：`f` 在磁盘现值上施加改动，返回写回后的配置
fn modify(f: impl FnOnce(&mut Config)) -> Config {
    let path = config_path();
    let mut cfg = merge_config(read_file_value(&path).as_ref());
    f(&mut cfg);
    write_config(&path, &cfg);
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
fn write_config(path: &Path, cfg: &Config) {
    let _guard = CONFIG_LOCK.write().unwrap_or_else(|e| e.into_inner());

    if let Some(parent) = path.parent() {
        _ = fs::create_dir_all(parent);
    }

    if let Ok(text) = serde_json::to_string_pretty(&cfg.values)
        && fs::write(path, format!("{text}\n")).is_ok()
    {
        CONFIG_CACHE.store(
            path,
            Value::Object(cfg.values.clone().into_iter().collect()),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 取临时 agent_dir 并加全局锁（配置读写走 agent_dir 单例）。
    fn setup() -> (
        crate::test_support::AgentDirGuard,
        std::sync::MutexGuard<'static, ()>,
    ) {
        let guard = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        (crate::test_support::AgentDirGuard::temp(), guard)
    }

    #[test]
    fn merge_fills_defaults_and_version() {
        let cfg = merge_config(None);
        assert_eq!(cfg.mode(), Mode::On);
        assert_eq!(cfg.inline_budget(), description::DEFAULT_INLINE_BUDGET);
        assert_eq!(cfg.values.get("configVersion"), Some(&json!(1)));
    }

    #[test]
    fn merge_reads_values_and_rejects_wrong_types() {
        let cfg = merge_config(Some(&json!({"mode": "only", "inlineBudget": 6000})));
        assert_eq!(cfg.mode(), Mode::Only);
        assert_eq!(cfg.inline_budget(), 6000);

        // 非字符串 / 负数 / 小数回落缺省
        let cfg = merge_config(Some(&json!({"mode": 1, "inlineBudget": -1})));
        assert_eq!(cfg.mode(), Mode::On);
        assert_eq!(cfg.inline_budget(), description::DEFAULT_INLINE_BUDGET);
        let cfg = merge_config(Some(&json!({"inlineBudget": 12.5})));
        assert_eq!(cfg.inline_budget(), description::DEFAULT_INLINE_BUDGET);
    }

    #[test]
    fn zero_budget_is_kept() {
        let cfg = merge_config(Some(&json!({"inlineBudget": 0})));
        assert_eq!(cfg.inline_budget(), 0);
    }

    #[test]
    fn unknown_mode_falls_back_to_on() {
        let cfg = merge_config(Some(&json!({"mode": "weird"})));
        assert_eq!(cfg.mode(), Mode::On);
    }

    #[test]
    fn load_creates_file_with_defaults() {
        let (_ad, _g) = setup();
        let cfg = load_config();
        assert_eq!(cfg.mode(), Mode::On);

        let text = fs::read_to_string(config_path()).unwrap();
        assert!(text.contains("\"mode\""), "got: {text}");
        assert!(text.contains("\"inlineBudget\""), "got: {text}");
        assert!(text.contains("\"configVersion\""), "got: {text}");
    }

    #[test]
    fn apply_panel_choice_persists_and_round_trips() {
        let (_ad, _g) = setup();

        apply_panel_choice("mode", "only").unwrap();
        apply_panel_choice("inlineBudget", "large").unwrap();

        let cfg = load_config();
        assert_eq!(cfg.mode(), Mode::Only);
        assert_eq!(cfg.inline_budget(), 6000);

        // 写回时保留另一个键与版本戳
        let text = fs::read_to_string(config_path()).unwrap();
        assert!(text.contains("\"mode\": \"only\""), "got: {text}");
        assert!(text.contains("\"configVersion\""), "got: {text}");
    }

    #[test]
    fn apply_panel_choice_rejects_unknown_key_and_value() {
        let (_ad, _g) = setup();
        assert!(apply_panel_choice("nope", "x").is_err());
        assert!(apply_panel_choice("mode", "nope").is_err());
        assert!(apply_panel_choice("inlineBudget", "nope").is_err());
    }

    #[test]
    fn panel_settings_expose_current_values() {
        let (_ad, _g) = setup();
        apply_panel_choice("mode", "only").unwrap();
        apply_panel_choice("inlineBudget", "none").unwrap();

        let settings = panel_settings();
        assert_eq!(settings.len(), 2);
        assert_eq!(settings[0].key, "mode");
        assert_eq!(settings[0].value, "only");
        assert_eq!(settings[1].key, "inlineBudget");
        assert_eq!(settings[1].value, "none");
    }

    #[test]
    fn corrupt_file_is_not_overwritten() {
        let (_ad, _g) = setup();
        let path = config_path();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{ not json").unwrap();

        // 读回缺省，但绝不覆盖用户（或半截写入的）文件
        assert_eq!(load_config().mode(), Mode::On);
        assert_eq!(fs::read_to_string(&path).unwrap(), "{ not json");
    }
}
