//! `recap` 的配置层：`agent_dir()/extensions/recap.json`。
//!
//! 目前只有一个键 `autoRecap`（`"on"` / `"off"`）：空闲 5 分钟后是否自动刷新一行会话小结。
//! `"on"`（缺省）时后台 tick 会发起小结请求；`"off"` 只保留 `/recap now` 的手动触发。
//!
//! 与 `notify` 等扩展一致：文件缺失 / 升级后缺键 / 版本戳不符时自动回写缺省值，
//! 损坏或非对象文件不覆盖（读回缺省）。

use crate::core::{config_cache::ConfigCache, settings_manager::agent_dir};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::RwLock,
};

/// 配置文件名前缀（位于 `agent_dir()/extensions/` 下）
pub(super) const CONFIG_FILE: &str = "recap.json";

/// 配置目录子路径
const CONFIG_SUBDIR: &str = "extensions";

/// 配置版本戳
const CONFIG_VERSION: i64 = 1;

/// 字符串缺省键：自动小结缺省开启（与扩展设置面板的 `on` / `off` 取值一致）。
pub(super) const STRING_DEFAULTS: &[(&str, &str)] = &[("autoRecap", "on")];

/// recap.json 读写锁（读配置与并发写盘）。
static CONFIG_LOCK: RwLock<()> = RwLock::new(());

/// recap.json 的缓存层（锁序恒为「RwLock → 缓存 Mutex」）。
static CONFIG_CACHE: ConfigCache = ConfigCache::new();

/// 运行期配置：键 → 字符串，缺省齐全。
#[derive(Clone)]
pub(super) struct Config {
    /// 配置键到 JSON 值的映射，含版本戳 configVersion；缺省键已在合并时补齐。
    values: BTreeMap<String, Value>,
}

impl Config {
    /// 按字符串读取（缺键或类型不符时回退空串）。
    pub(super) fn text(&self, key: &str) -> String {
        self.values
            .get(key)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    }

    /// 按开关读取：`off` / `false` / `0`（忽略大小写与首尾空白）为假，其余（含缺省）为真。
    pub(super) fn flag(&self, key: &str) -> bool {
        !matches!(
            self.text(key).trim().to_ascii_lowercase().as_str(),
            "off" | "false" | "0"
        )
    }
}

/// 全部缺省键（含 `configVersion` 之外的键；用于回填判断）。
pub(super) fn default_keys() -> Vec<String> {
    STRING_DEFAULTS.iter().map(|(k, _)| k.to_string()).collect()
}

/// 缺省 + 文件覆盖合并（纯函数，便于测试）。
///
/// 只接受字符串与布尔；布尔折算为 `on` / `off`（兼容手写成 `true` / `false` 的配置），
/// 其余类型回落缺省（不让一次手误把配置读成 `off`）。
pub(super) fn merge_config(from_file: Option<&Value>) -> Config {
    let mut values: BTreeMap<String, Value> = BTreeMap::new();
    for (key, default) in STRING_DEFAULTS {
        let value = from_file
            .and_then(|f| f.get(key))
            .and_then(read_scalar)
            .unwrap_or_else(|| (*default).to_string());
        values.insert((*key).to_string(), json!(value));
    }
    values.insert("configVersion".to_string(), json!(CONFIG_VERSION));
    Config { values }
}

/// 读一个标量键：字符串原样，布尔折算成 `on` / `off`，其余类型返回 `None`（回落缺省）。
fn read_scalar(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Bool(b) => Some(if *b { "on" } else { "off" }.to_string()),
        _ => None,
    }
}

/// 覆盖写入一个字符串键（保留其它键）。
fn write_key(key: &str, value: &str) -> Result<(), String> {
    let path = config_path();
    let from_file = read_file_value(&path);
    let mut cfg = merge_config(from_file.as_ref());
    cfg.values.insert(key.to_string(), json!(value));
    write_config(&path, &cfg)
}

/// 写入自动小结开关（`on` / `off`），供 `/recap on|off` 与扩展设置面板使用。
pub(super) fn set_auto_recap(on: bool) -> Result<(), String> {
    write_key("autoRecap", if on { "on" } else { "off" })
}

/// 自动小结是否开启（缺省键已由 [`merge_config`] 补为 `on`）。
pub(super) fn auto_recap() -> bool {
    load_config().flag("autoRecap")
}

/// 配置文件路径：`agent_dir()/extensions/recap.json`
pub(super) fn config_path() -> PathBuf {
    agent_dir().join(CONFIG_SUBDIR).join(CONFIG_FILE)
}

/// 从磁盘加载配置；文件缺失 / 缺键 / 版本戳不符时回写（损坏文件不覆盖）。
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
        _ = write_config(&path, &cfg);
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

/// 写盘（camelCase 键）。成功后回填 [`CONFIG_CACHE`]；失败返回可直接展示的错误文案。
fn write_config(path: &Path, cfg: &Config) -> Result<(), String> {
    let _guard = CONFIG_LOCK.write().unwrap_or_else(|e| e.into_inner());

    if let Some(parent) = path.parent()
        && let Err(e) = fs::create_dir_all(parent)
    {
        return Err(format!("{e}"));
    }

    let text = serde_json::to_string_pretty(&cfg.values).map_err(|e| format!("{e}"))?;
    fs::write(path, format!("{text}\n")).map_err(|e| format!("{e}"))?;
    CONFIG_CACHE.store(
        path,
        Value::Object(cfg.values.clone().into_iter().collect()),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_fills_defaults_and_version() {
        let cfg = merge_config(None);
        assert_eq!(cfg.text("autoRecap"), "on");
        assert!(cfg.flag("autoRecap"), "缺省开启");
        assert_eq!(cfg.values.get("configVersion"), Some(&json!(1)));
    }

    #[test]
    fn merge_reads_string_and_rejects_wrong_type() {
        let cfg = merge_config(Some(&json!({ "autoRecap": "off" })));
        assert!(!cfg.flag("autoRecap"));

        // 旧格式 / 手写的布尔值折算为 on / off
        let cfg = merge_config(Some(&json!({ "autoRecap": false })));
        assert!(!cfg.flag("autoRecap"), "布尔 false 折算为 off");
        let cfg = merge_config(Some(&json!({ "autoRecap": true })));
        assert!(cfg.flag("autoRecap"));

        // 非标量回落缺省（开启）
        let cfg = merge_config(Some(&json!({ "autoRecap": 1 })));
        assert!(cfg.flag("autoRecap"), "类型不符回落缺省");
    }

    #[test]
    fn default_keys_lists_auto_recap() {
        assert_eq!(default_keys(), vec!["autoRecap".to_string()]);
    }

    #[test]
    fn set_auto_recap_persists_and_round_trips() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();

        set_auto_recap(false).unwrap();
        assert!(!auto_recap());

        // 落盘内容确实包含该键（且保留 configVersion）
        let text = fs::read_to_string(config_path()).unwrap();
        assert!(text.contains("\"autoRecap\""), "got: {text}");
        assert!(text.contains("\"configVersion\""), "got: {text}");

        set_auto_recap(true).unwrap();
        assert!(auto_recap());
    }
}
