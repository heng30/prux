//! hang-detect 的配置层：缺省值、类型/范围校验、面板档位映射与 JSON 落盘。
//!
//! 配置持久化在 `agent_dir()/extensions/hang-detect.json`（首次加载自动创建并回填缺省键）。
//! 三个键（camelCase）：
//!
//! | 键 | 含义 | 缺省 | `0` 的语义 |
//! |----|------|------|-----------|
//! | `timeoutSecs` | 等待模型（非工具执行）时的最大静默时长 | 180 | 关闭看门狗 |
//! | `toolTimeoutSecs` | 单个工具执行期间的最大静默时长 | 0 | 工具执行期间不检查 |
//! | `maxRecoveries` | 一次「未成功结算」的连续恢复次数上限 | 3 | 不设上限 |
//!
//! `/hang-detect` 打开扩展设置面板（无参数；预设档位、Space/Enter 循环切换、立即落盘）；
//! `/hang-detect timeout|tool-timeout|recoveries <n>` 写入任意数值。

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

/// 扩展名（注册名，也是配置目录下的文件名前缀）
pub(super) const EXT: &str = "hang-detect";
/// 斜杠命令名（`/hang-detect`）
pub(super) const CMD: &str = "hang-detect";
/// 扩展配置所在子目录
const CONFIG_SUBDIR: &str = "extensions";
/// 配置文件名（位于 `agent_dir()/extensions/` 下）
pub(super) const CONFIG_FILE: &str = "hang-detect.json";
/// 配置版本戳
const CONFIG_VERSION: i64 = 1;

/// 面板档位表：`(内部数值, 面板标签)`。
///
/// `timeoutSecs` 档位（等待模型时的静默上限；越大越宽松）
const TIMEOUT_TABLE: &[(f64, &str)] = &[
    (0.0, "off"),
    (30.0, "30s"),
    (60.0, "1m"),
    (120.0, "2m"),
    (180.0, "3m"),
    (300.0, "5m"),
    (600.0, "10m"),
];
/// `toolTimeoutSecs` 档位（工具执行期间的静默上限）
const TOOL_TIMEOUT_TABLE: &[(f64, &str)] =
    &[(0.0, "off"), (300.0, "5m"), (600.0, "10m"), (1800.0, "30m")];

/// `maxRecoveries` 档位（连续恢复次数上限）
const MAX_RECOVERIES_TABLE: &[(f64, &str)] =
    &[(0.0, "unlimited"), (1.0, "1"), (3.0, "3"), (5.0, "5")];

/// 数值缺省（`0` 的语义见模块文档）。
pub(super) const NUMERIC_DEFAULTS: &[(&str, f64)] = &[
    ("timeoutSecs", 180.0),
    ("toolTimeoutSecs", 0.0),
    ("maxRecoveries", 3.0),
];

/// hang-detect.json 读写锁（面板应用设置与读配置可能并发）。
static CONFIG_LOCK: RwLock<()> = RwLock::new(());

/// hang-detect.json 的缓存层。读/写都只在 [`CONFIG_LOCK`] 作用域内进行，
/// 锁序恒为「RwLock → 缓存 Mutex」。
static CONFIG_CACHE: ConfigCache = ConfigCache::new();

/// 扩展配置目录 `agent_dir()/extensions/`
pub(super) fn config_path() -> PathBuf {
    agent_dir().join(CONFIG_SUBDIR).join(CONFIG_FILE)
}

/// 面板项（顺序即面板显示顺序）。
///
/// 逐项用 [`cycle_setting`] 现构 [`ExtensionSetting`]：面板键 → 内部数值键的映射仍由
/// [`panel_value`] 负责，这里只把「键、展示名、说明、当前值、档位表」摆到一起。
pub(super) fn panel_settings(cfg: &Config) -> Vec<ExtensionSetting> {
    vec![
        cycle_setting(
            "timeout",
            "Idle timeout",
            "Abort and restart when the model produces no activity for this long (off disables the watchdog)",
            panel_value(cfg, "timeout"),
            &["off", "30s", "1m", "2m", "3m", "5m", "10m"],
        ),
        cycle_setting(
            "toolTimeout",
            "Tool timeout",
            "Abort and restart when a running tool produces no activity for this long (off = long tools are never watched)",
            panel_value(cfg, "toolTimeout"),
            &["off", "5m", "10m", "30m"],
        ),
        cycle_setting(
            "maxRecoveries",
            "Max recoveries",
            "How many times to restart after consecutive stalls before giving up (reset by a normal settle)",
            panel_value(cfg, "maxRecoveries"),
            &["unlimited", "1", "3", "5"],
        ),
    ]
}

/// 运行期配置：键 → 数值，缺省齐全。
#[derive(Clone)]
pub(super) struct Config {
    /// 扁平配置表：配置键 → JSON 值（数值键存整数），由缺省值与文件覆盖合并而来。
    pub(super) values: BTreeMap<String, Value>,
}

impl Config {
    /// 按数值读取（缺键或类型不符时回退 `0.0`）
    pub(super) fn num(&self, key: &str) -> f64 {
        self.values.get(key).and_then(|v| v.as_f64()).unwrap_or(0.0)
    }

    /// 按非负整数读取（负数/非数/缺键时回退 `0`）
    pub(super) fn int(&self, key: &str) -> usize {
        self.values
            .get(key)
            .and_then(|v| v.as_f64())
            .filter(|v| *v >= 0.0)
            .map(|v| v as usize)
            .unwrap_or(0)
    }
}

/// 全部缺省键（用于回填判断）
pub(super) fn default_keys() -> Vec<String> {
    NUMERIC_DEFAULTS
        .iter()
        .map(|(k, _)| k.to_string())
        .collect()
}

/// 由内部数值键反查面板标签（不在表中时回退到原始数字）。
fn label_for(cfg: &Config, key: &str, table: &[(f64, &str)]) -> String {
    let current = cfg.num(key);
    table
        .iter()
        .find(|(v, _)| (*v - current).abs() < f64::EPSILON)
        .map(|(_, l)| (*l).to_string())
        .unwrap_or_else(|| format!("{current}"))
}

/// 读当前值 → 面板标签。
pub(super) fn panel_value(cfg: &Config, key: &str) -> String {
    match key {
        "timeout" => label_for(cfg, "timeoutSecs", TIMEOUT_TABLE),
        "toolTimeout" => label_for(cfg, "toolTimeoutSecs", TOOL_TIMEOUT_TABLE),
        "maxRecoveries" => label_for(cfg, "maxRecoveries", MAX_RECOVERIES_TABLE),
        _ => String::new(),
    }
}

/// 应用面板选择（写回内部数值键）。
pub(super) fn apply_panel_choice(
    cfg: &mut Config,
    key: &str,
    value: &str,
) -> std::result::Result<(), String> {
    match key {
        "timeout" => set_numeric(cfg, "timeoutSecs", choice_num(TIMEOUT_TABLE, value)?),
        "toolTimeout" => set_numeric(
            cfg,
            "toolTimeoutSecs",
            choice_num(TOOL_TIMEOUT_TABLE, value)?,
        ),
        "maxRecoveries" => set_numeric(
            cfg,
            "maxRecoveries",
            choice_num(MAX_RECOVERIES_TABLE, value)?,
        ),
        other => return Err(format!("unknown setting: {other}")),
    }
    Ok(())
}

/// 写入一个数值键（供面板与 `/hang-detect <key> <n>` 共用）。
pub(super) fn set_numeric(cfg: &mut Config, key: &str, value: f64) {
    cfg.values.insert(key.to_string(), json!(value as i64));
}

/// 把候选值按键的类型/范围校验后返回可存值；不合法则 `None`（回落缺省）。
///
/// 本扩展只有非负整数键：`0` 是合法值（`off` / `unlimited`），负数与小数拒绝。
fn validated_value(value: &Value) -> Option<Value> {
    let v = value.as_f64()?;
    if !v.is_finite() || v < 0.0 || v.fract() != 0.0 {
        return None;
    }
    Some(json!(v as i64))
}

/// 缺省 + 文件覆盖合并（纯函数，便于测试）。
pub(super) fn merge_config(from_file: Option<&Value>) -> Config {
    let mut values: BTreeMap<String, Value> = BTreeMap::new();
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

/// 从磁盘加载配置；文件缺失/升级后缺键/版本戳不符时回写（损坏文件不覆盖）。
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
        // 注意：不能在持有读锁时调用（`RwLock` 不可重入，读锁内取写锁会自死锁）。
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

/// 写盘：键名一律 camelCase（如 `timeoutSecs`、`maxRecoveries`）。
///
/// 落盘成功后回填 [`CONFIG_CACHE`]（取写盘后的元数据），下次读取直接命中。
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
