//! `notify` 的配置层：`agent_dir()/extensions/notify.json`。
//!
//! 目前只有一个键 `settledCommand`：agent 从忙碌转为空闲
//! （整轮 settle）时执行的外部命令模板，其中 `${message}` 会被替换为本次结算的
//! 通知文本。空串（缺省）= 不执行任何命令。
//!
//! 与其它扩展一致：文件缺失 / 升级后缺键 / 版本戳不符时自动回写缺省值，
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
pub(super) const CONFIG_FILE: &str = "notify.json";

/// 配置目录子路径
const CONFIG_SUBDIR: &str = "extensions";

/// 配置版本戳
const CONFIG_VERSION: i64 = 1;

/// 字符串缺省键
pub(super) const STRING_DEFAULTS: &[(&str, &str)] = &[("settledCommand", "")];

/// notify.json 读写锁（读配置与并发写盘）。
static CONFIG_LOCK: RwLock<()> = RwLock::new(());

/// notify.json 的缓存层（锁序恒为「RwLock → 缓存 Mutex」）。
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
}

/// 全部缺省键（含 `configVersion` 之外的键；用于回填判断）。
pub(super) fn default_keys() -> Vec<String> {
    STRING_DEFAULTS.iter().map(|(k, _)| k.to_string()).collect()
}

/// 缺省 + 文件覆盖合并（纯函数，便于测试）。
///
/// 只接受字符串类型；类型不符的键回落缺省（不让一次手误把配置读成空）。
pub(super) fn merge_config(from_file: Option<&Value>) -> Config {
    let mut values: BTreeMap<String, Value> = BTreeMap::new();
    for (key, default) in STRING_DEFAULTS {
        let value = from_file
            .and_then(|f| f.get(key))
            .and_then(|v| v.as_str())
            .map(|s| json!(s))
            .unwrap_or_else(|| json!(default));
        values.insert((*key).to_string(), value);
    }
    values.insert("configVersion".to_string(), json!(CONFIG_VERSION));
    Config { values }
}

/// 覆盖写入一个字符串键（保留其它键）。
fn write_key(key: &str, value: &str) {
    let path = config_path();
    let from_file = read_file_value(&path);
    let mut cfg = merge_config(from_file.as_ref());
    cfg.values.insert(key.to_string(), json!(value));
    write_config(&path, &cfg);
}

/// 覆盖写入 `settledCommand`（保留其它键），供 `/notify settled <command>` 使用。
pub(super) fn set_settled_command(cmd: &str) {
    write_key("settledCommand", cmd);
}

/// 清空 `settledCommand`（回落缺省空串，保留其它键），供 `/notify reset settled` 使用。
pub(super) fn reset_settled_command() {
    write_key("settledCommand", "");
}

/// 清空全部命令键（当前仅 `settledCommand`），供 `/notify reset all` 使用。
///
/// 按 [`STRING_DEFAULTS`] 遍历，后续新增命令键会自动纳入 `all` 的清理范围。
pub(super) fn reset_all() {
    let path = config_path();
    let from_file = read_file_value(&path);
    let mut cfg = merge_config(from_file.as_ref());

    for (key, default) in STRING_DEFAULTS {
        cfg.values.insert((*key).to_string(), json!(default));
    }

    write_config(&path, &cfg);
}

/// 配置文件路径：`agent_dir()/extensions/notify.json`
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

    #[test]
    fn merge_fills_defaults_and_version() {
        let cfg = merge_config(None);
        assert_eq!(cfg.text("settledCommand"), "");
        assert_eq!(cfg.values.get("configVersion"), Some(&json!(1)));
    }

    #[test]
    fn merge_reads_string_and_rejects_wrong_type() {
        let cfg = merge_config(Some(&json!({ "settledCommand": "notify-send hi" })));
        assert_eq!(cfg.text("settledCommand"), "notify-send hi");

        // 非字符串回落缺省
        let cfg = merge_config(Some(&json!({ "settledCommand": 42 })));
        assert_eq!(cfg.text("settledCommand"), "");
    }

    #[test]
    fn default_keys_lists_settled_command() {
        assert_eq!(default_keys(), vec!["settledCommand".to_string()]);
    }

    #[test]
    fn set_settled_command_persists_and_round_trips() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();

        set_settled_command("notify-send prux ${message}");
        assert_eq!(
            load_config().text("settledCommand"),
            "notify-send prux ${message}"
        );

        // 落盘内容确实包含该键（且保留 configVersion）
        let text = fs::read_to_string(config_path()).unwrap();
        assert!(text.contains("\"settledCommand\""), "got: {text}");
        assert!(text.contains("\"configVersion\""), "got: {text}");
    }

    #[test]
    fn reset_clears_commands_and_keeps_version() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();

        set_settled_command("notify-send hi ${message}");
        reset_settled_command();
        assert_eq!(load_config().text("settledCommand"), "");

        set_settled_command("notify-send hi ${message}");
        reset_all();
        assert_eq!(load_config().text("settledCommand"), "");
        // 清空不应抹掉版本戳
        let text = fs::read_to_string(config_path()).unwrap();
        assert!(text.contains("\"configVersion\""), "got: {text}");
    }
}
