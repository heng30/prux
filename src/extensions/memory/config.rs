//! memory 扩展的配置层：缺省值、面板档位映射与 `agent_dir()/extensions/memory.json` 落盘。
//!
//! 三个键（camelCase）：
//!
//! | 键 | 含义 | 缺省 | `0` 的语义 |
//! |----|------|------|-----------|
//! | `recallLimit` | 每轮自动注入的记忆条数上限 | 5 | 关闭自动召回 |
//! | `extractInterval` | 每多少次 `turn_end` 触发一次抽取 | 12 | 关闭自动抽取 |
//! | `injectBudgetChars` | 注入块的字符预算 | 8000 | 关闭注入（等价于关闭自动召回） |
//!
//! 记忆**存储**不在这个文件里：条目分别在
//! `agent_dir()/extensions/memory/global.json` 与 `.../projects/<project-key>.json`。

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

/// 扩展名（注册名，也是 `/extension` 面板里的标识）。
pub(super) const EXT: &str = "memory";

/// 斜杠命令名。
pub(super) const CMD: &str = "memory";

/// 扩展配置所在子目录。
const CONFIG_SUBDIR: &str = "extensions";

/// 配置文件名（位于 `agent_dir()/extensions/` 下）。
pub(super) const CONFIG_FILE: &str = "memory.json";

/// 配置版本戳。
const CONFIG_VERSION: i64 = 1;

/// `recallLimit` 的面板档位（内部数值, 面板标签）。
const RECALL_TABLE: &[(f64, &str)] = &[
    (0.0, "off"),
    (3.0, "3"),
    (5.0, "5"),
    (8.0, "8"),
    (12.0, "12"),
];

/// `extractInterval` 的面板档位（单位：`turn_end` 次数，`0` = 关闭）。
const EXTRACT_TABLE: &[(f64, &str)] = &[
    (0.0, "off"),
    (6.0, "6 turns"),
    (12.0, "12 turns"),
    (24.0, "24 turns"),
    (48.0, "48 turns"),
];

/// `injectBudgetChars` 的面板档位（单位：字符）。
const BUDGET_TABLE: &[(f64, &str)] = &[
    (2000.0, "2k"),
    (4000.0, "4k"),
    (8000.0, "8k"),
    (16000.0, "16k"),
];

/// 面板档位的键 → 配置键。
const RECALL_KEY: &str = "recallLimit";
/// 抽取间隔在配置里的键。
const EXTRACT_KEY: &str = "extractInterval";
/// 注入预算在配置里的键。
const BUDGET_KEY: &str = "injectBudgetChars";

/// 数值缺省（`0` 的语义见模块文档）。
pub(super) const NUMERIC_DEFAULTS: &[(&str, f64)] =
    &[(RECALL_KEY, 5.0), (EXTRACT_KEY, 12.0), (BUDGET_KEY, 8000.0)];

/// memory.json 的读写锁（面板应用设置与读配置可能并发）。
static CONFIG_LOCK: RwLock<()> = RwLock::new(());

/// memory.json 的缓存层；只在 [`CONFIG_LOCK`] 作用域内读写（锁序恒为「RwLock → 缓存 Mutex」）。
static CONFIG_CACHE: ConfigCache = ConfigCache::new();

/// 配置文件路径 `agent_dir()/extensions/memory.json`。
pub(super) fn config_path() -> PathBuf {
    agent_dir().join(CONFIG_SUBDIR).join(CONFIG_FILE)
}

/// 运行期配置：键 → 数值，缺省齐全。
#[derive(Clone)]
pub(super) struct Config {
    /// 扁平配置表（数值键存整数）。
    pub(super) values: BTreeMap<String, Value>,
}

impl Config {
    /// 按非负整数读取（缺键/非数/负数回退 `0`）。
    fn uint(&self, key: &str) -> usize {
        self.values
            .get(key)
            .and_then(|v| v.as_f64())
            .filter(|v| *v >= 0.0)
            .map(|v| v as usize)
            .unwrap_or(0)
    }

    /// 每轮注入的记忆条数上限；`0` = 关闭自动召回。
    pub(super) fn recall_limit(&self) -> usize {
        self.uint(RECALL_KEY)
    }

    /// 自动抽取的间隔（`turn_end` 次数）；`0` = 关闭自动抽取。
    pub(super) fn extract_interval(&self) -> usize {
        self.uint(EXTRACT_KEY)
    }

    /// 注入块的字符预算。
    pub(super) fn inject_budget(&self) -> usize {
        self.uint(BUDGET_KEY)
    }
}

/// 全部缺省键（用于回填判断）。
fn default_keys() -> Vec<String> {
    NUMERIC_DEFAULTS
        .iter()
        .map(|(k, _)| k.to_string())
        .collect()
}

/// 面板项（顺序即面板显示顺序）。
pub(super) fn panel_settings(cfg: &Config) -> Vec<ExtensionSetting> {
    vec![
        cycle_setting(
            "recall",
            "Auto recall",
            "How many memories to inject before each request (off disables automatic recall)",
            label_for(cfg, RECALL_KEY, RECALL_TABLE),
            &["off", "3", "5", "8", "12"],
        ),
        cycle_setting(
            "extract",
            "Auto extract",
            "Extract durable facts from the conversation every N turns (off disables extraction)",
            label_for(cfg, EXTRACT_KEY, EXTRACT_TABLE),
            &["off", "6 turns", "12 turns", "24 turns", "48 turns"],
        ),
        cycle_setting(
            "budget",
            "Inject budget",
            "Character budget for the injected memory block",
            label_for(cfg, BUDGET_KEY, BUDGET_TABLE),
            &["2k", "4k", "8k", "16k"],
        ),
    ]
}

/// 由内部数值反查面板标签（不在表中时回退原始数字）。
fn label_for(cfg: &Config, key: &str, table: &[(f64, &str)]) -> String {
    let current = cfg.uint(key) as f64;
    table
        .iter()
        .find(|(v, _)| (*v - current).abs() < f64::EPSILON)
        .map(|(_, l)| (*l).to_string())
        .unwrap_or_else(|| format!("{current}"))
}

/// 应用一项面板选择（写回内部数值键）。
pub(super) fn apply_setting(
    cfg: &mut Config,
    key: &str,
    value: &str,
) -> std::result::Result<(), String> {
    match key {
        "recall" => set_numeric(cfg, RECALL_KEY, choice_num(RECALL_TABLE, value)?),
        "extract" => set_numeric(cfg, EXTRACT_KEY, choice_num(EXTRACT_TABLE, value)?),
        "budget" => set_numeric(cfg, BUDGET_KEY, choice_num(BUDGET_TABLE, value)?),
        other => return Err(format!("unknown setting: {other}")),
    }
    Ok(())
}

/// 写入一个数值键（非负整数）。
pub(super) fn set_numeric(cfg: &mut Config, key: &str, value: f64) {
    cfg.values.insert(key.to_string(), json!(value as i64));
}

/// 候选值的类型/范围校验：只接受非负整数（`0` 合法）。
fn validated_value(value: &Value) -> Option<Value> {
    let v = value.as_f64()?;
    if !v.is_finite() || v < 0.0 || v.fract() != 0.0 {
        return None;
    }
    Some(json!(v as i64))
}

/// 缺省 + 文件覆盖合并（纯函数，便于测试）。
pub(super) fn merge_config(from_file: Option<&Value>) -> Config {
    let mut values = BTreeMap::new();
    for (key, default) in NUMERIC_DEFAULTS {
        let value = from_file
            .and_then(|f| f.get(key))
            .and_then(validated_value)
            .unwrap_or_else(|| json!(*default as i64));
        values.insert((*key).to_string(), value);
    }
    values.insert("configVersion".to_string(), json!(CONFIG_VERSION));
    Config { values }
}

/// 从磁盘加载配置；缺键/版本戳不符时回写（损坏文件不覆盖）。
pub(super) fn load_config() -> Config {
    let path = config_path();
    let from_file = read_file_value(&path);
    let cfg = merge_config(from_file.as_ref());

    let should_write = match &from_file {
        None => !path.exists(),
        Some(file) => {
            default_keys().iter().any(|k| file.get(k).is_none())
                || file.get("configVersion").and_then(Value::as_i64) != Some(CONFIG_VERSION)
        }
    };
    if should_write {
        // 不能在持读锁时调用（RwLock 不可重入）。
        write_config(&path, &cfg);
    }
    cfg
}

/// 读配置文件的 JSON 对象（不存在/非法/非对象 → `None`）。
fn read_file_value(path: &Path) -> Option<Value> {
    let _guard = CONFIG_LOCK.read().unwrap_or_else(|e| e.into_inner());
    let value = CONFIG_CACHE.read(path, || {
        fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .filter(Value::is_object)
            .unwrap_or(Value::Null)
    });
    (!value.is_null()).then_some(value)
}

/// 写盘并回填缓存。
pub(super) fn write_config(path: &Path, cfg: &Config) {
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
    use crate::test_support::AgentDirGuard;

    #[test]
    fn defaults_are_complete_and_merge_validates_types() {
        let cfg = merge_config(None);
        assert_eq!(cfg.recall_limit(), 5);
        assert_eq!(cfg.extract_interval(), 12);
        assert_eq!(cfg.inject_budget(), 8000);
        assert_eq!(cfg.values["configVersion"], json!(CONFIG_VERSION));

        let cfg = merge_config(Some(&json!({
            "recallLimit": 8,
            "extractInterval": -3,
            "injectBudgetChars": 2.5,
            "configVersion": 1
        })));
        assert_eq!(cfg.recall_limit(), 8, "合法值被采用");
        assert_eq!(cfg.extract_interval(), 12, "负数回退缺省");
        assert_eq!(cfg.inject_budget(), 8000, "小数回退缺省");
    }

    #[test]
    fn zero_disables_features() {
        let cfg = merge_config(Some(&json!({
            "recallLimit": 0,
            "extractInterval": 0,
            "injectBudgetChars": 0,
            "configVersion": 1
        })));
        assert_eq!(cfg.recall_limit(), 0);
        assert_eq!(cfg.extract_interval(), 0);
        assert_eq!(cfg.inject_budget(), 0);
    }

    #[test]
    fn panel_settings_cycle_through_all_choices() {
        let mut cfg = merge_config(None);
        let items = panel_settings(&cfg);
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].value, "5");
        assert_eq!(items[1].value, "12 turns");
        assert_eq!(items[2].value, "8k");

        apply_setting(&mut cfg, "recall", "off").unwrap();
        apply_setting(&mut cfg, "extract", "6 turns").unwrap();
        apply_setting(&mut cfg, "budget", "2k").unwrap();
        assert_eq!(cfg.recall_limit(), 0);
        assert_eq!(cfg.extract_interval(), 6);
        assert_eq!(cfg.inject_budget(), 2000);

        assert_eq!(
            apply_setting(&mut cfg, "recall", "bogus"),
            Err("invalid option: bogus".to_string())
        );
        assert_eq!(
            apply_setting(&mut cfg, "nope", "1"),
            Err("unknown setting: nope".to_string())
        );
    }

    #[test]
    fn config_round_trips_and_backfills_missing_keys() {
        let _guard = AgentDirGuard::temp();
        let cfg = load_config();
        assert_eq!(cfg.recall_limit(), 5, "首次加载写回缺省");

        let path = config_path();
        assert!(path.exists());
        std::fs::write(&path, r#"{"recallLimit": 3}"#).unwrap();
        let cfg = load_config();
        assert_eq!(cfg.recall_limit(), 3, "已有值被保留");
        assert_eq!(cfg.extract_interval(), 12, "缺键回填缺省");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("extractInterval"), "缺键被写回: {text}");
        assert!(text.contains("configVersion"));
    }

    #[test]
    fn corrupt_config_falls_back_without_overwriting() {
        let _guard = AgentDirGuard::temp();
        let path = config_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{broken").unwrap();

        let cfg = load_config();
        assert_eq!(cfg.recall_limit(), 5);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{broken",
            "损坏文件不覆盖（用户要能看到自己写坏了什么）"
        );
    }
}
