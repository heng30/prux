//! loop-detect 的配置层：缺省值、类型/范围校验、面板档位映射与 JSON 落盘。
//!
//! 配置持久化在 `agent_dir()/extensions/loop-detect.json`（首次加载自动创建并回填缺省键）。
//! 任意检测器把对应键设为 `0` 即关闭：字符级窗口 `thinkingWindow` / `outputWindow`、
//! 语义阈值 `semanticThreshold`、跨轮停滞 `stagnationWindow`、工具序列 `toolLoop`。

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
pub(super) const EXT: &str = "loop-detect";
/// 斜杠命令名（`/loop-detect`）
pub(super) const CMD: &str = "loop-detect";
/// 扩展配置所在子目录
const CONFIG_SUBDIR: &str = "extensions";
/// 配置文件名（位于 `agent_dir()/extensions/` 下）
pub(super) const CONFIG_FILE: &str = "loop-detect.json";
/// 配置版本戳
const CONFIG_VERSION: i64 = 1;

/// 档位映射表：`(内部数值, 面板标签)`。
///
/// `semanticThreshold` 档位（段落指纹重复多少次判定语义循环）
const SEMANTIC_TABLE: &[(f64, &str)] = &[(0.0, "off"), (3.0, "normal"), (2.0, "aggressive")];
/// `stagnationWindow` 档位（连续多少轮思考相似才判定停滞）
const STAGNATION_TABLE: &[(f64, &str)] = &[(0.0, "off"), (4.0, "normal"), (3.0, "aggressive")];
/// `toolLoop` 档位（是否检测重复的工具调用序列）
const TOOL_LOOP_TABLE: &[(f64, &str)] = &[(0.0, "off"), (1.0, "on")];
/// `thinkingWindow` 档位（思考字符级循环窗口；越大越宽松）
const THINKING_WINDOW_TABLE: &[(f64, &str)] = &[
    (0.0, "off"),
    (160.0, "loose"),
    (80.0, "normal"),
    (40.0, "tight"),
];
/// `outputWindow` 档位（可见回复字符级循环窗口）
const OUTPUT_WINDOW_TABLE: &[(f64, &str)] = &[
    (0.0, "off"),
    (200.0, "loose"),
    (100.0, "normal"),
    (50.0, "tight"),
];

/// 面板项（顺序即面板显示顺序）：覆盖全部 5 个检测器。
///
/// 逐项用 [`cycle_setting`] 现构 [`ExtensionSetting`]：面板键 → 内部数值键的映射仍由
/// [`panel_value`] 负责，这里只把「键、展示名、说明、当前值、档位表」摆到一起。
pub(super) fn panel_settings(cfg: &Config) -> Vec<ExtensionSetting> {
    vec![
        cycle_setting(
            "streamThinking",
            "Thinking loop",
            "Character-level thinking repetition detection (off to disable; loose is more permissive, tight more sensitive)",
            panel_value(cfg, "streamThinking"),
            &["off", "loose", "normal", "tight"],
        ),
        cycle_setting(
            "streamOutput",
            "Output loop",
            "Character-level visible-reply repetition detection",
            panel_value(cfg, "streamOutput"),
            &["off", "loose", "normal", "tight"],
        ),
        cycle_setting(
            "semanticLoop",
            "Semantic loop",
            "Paragraph fingerprint repetition detection (thinking + output)",
            panel_value(cfg, "semanticLoop"),
            &["off", "normal", "aggressive"],
        ),
        cycle_setting(
            "stagnation",
            "Reasoning stagnation",
            "Terminate when thinking is highly similar across turns (only compared once thinking is long enough)",
            panel_value(cfg, "stagnation"),
            &["off", "normal", "aggressive"],
        ),
        cycle_setting(
            "toolLoop",
            "Tool call loop",
            "Terminate when the same tool call sequence repeats identically",
            panel_value(cfg, "toolLoop"),
            &["off", "on"],
        ),
    ]
}

/// 数值缺省（`0 = 关闭` 的语义见配置键注释）。
///
/// 仅保留“用户真正需要调”的档位/阈值；内部实现细节（检测步长、扫描上限、
/// 段落最小长度、指纹长度）一律推导，不再暴露成配置键。
pub(super) const NUMERIC_DEFAULTS: &[(&str, f64)] = &[
    ("thinkingWindow", 80.0),
    ("outputWindow", 100.0),
    ("semanticThreshold", 3.0),
    ("stagnationWindow", 4.0),
    ("stagnationThreshold", 0.85),
    ("toolLoop", 1.0),
    ("hookTimeoutMs", 5000.0),
];

/// 字符串缺省
pub(super) const STRING_DEFAULTS: &[(&str, &str)] =
    &[("toolLoopExempt", ""), ("hookCmd", ""), ("hookLog", "")];

/// 相似度阈值类（0..=1 浮点）
const RATIO_KEYS: &[&str] = &["stagnationThreshold"];
/// 这类整数键不接受 0（`0` 不是「关闭」而是非法）
const POSITIVE_KEYS: &[&str] = &["hookTimeoutMs"];

/// 是否至少启用了一个检测器。全部关闭时本扩展无事可做，[`super::detect::interest_for`]
/// 返回空，框架便**完全跳过**本扩展（含每轮的 TransformContext 探针）。
///
/// 键序按常见配置短路：默认档位下首个 `thinkingWindow` 即命中，
/// 典型仅一次 BTreeMap 查表（不缓存：遵循 plan-mode“实时求值、杜绝忘记刷新”的思路）。
pub(super) fn any_detector_enabled(cfg: &Config) -> bool {
    // 流式检测：字符窗口或语义阈值任一开启
    let stream = cfg.int("thinkingWindow") > 0
        || cfg.int("outputWindow") > 0
        || cfg.num("semanticThreshold") > 0.0;

    // 跨轮停滞
    let cross_turn = cfg.int("stagnationWindow") > 0;

    // 工具级检测
    let tools = cfg.int("toolLoop") > 0;
    stream || cross_turn || tools
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
        "streamThinking" => label_for(cfg, "thinkingWindow", THINKING_WINDOW_TABLE),
        "streamOutput" => label_for(cfg, "outputWindow", OUTPUT_WINDOW_TABLE),
        "semanticLoop" => label_for(cfg, "semanticThreshold", SEMANTIC_TABLE),
        "stagnation" => label_for(cfg, "stagnationWindow", STAGNATION_TABLE),
        "toolLoop" => label_for(cfg, "toolLoop", TOOL_LOOP_TABLE),
        _ => String::new(),
    }
}

/// 应用面板选择（写回内部数值键）。
pub(super) fn apply_panel_choice(
    cfg: &mut Config,
    key: &str,
    value: &str,
) -> std::result::Result<(), String> {
    let set = |cfg: &mut Config, k: &str, v: f64| {
        cfg.values.insert(k.to_string(), json!(v));
    };
    match key {
        "streamThinking" => set(
            cfg,
            "thinkingWindow",
            choice_num(THINKING_WINDOW_TABLE, value)?,
        ),
        "streamOutput" => set(cfg, "outputWindow", choice_num(OUTPUT_WINDOW_TABLE, value)?),
        "semanticLoop" => set(cfg, "semanticThreshold", choice_num(SEMANTIC_TABLE, value)?),
        "stagnation" => set(
            cfg,
            "stagnationWindow",
            choice_num(STAGNATION_TABLE, value)?,
        ),
        "toolLoop" => set(cfg, "toolLoop", choice_num(TOOL_LOOP_TABLE, value)?),
        other => return Err(format!("unknown setting: {other}")),
    }
    Ok(())
}

/// 运行期配置：键 → 数值 / 字符串，缺省齐全。
#[derive(Clone)]
pub(super) struct Config {
    /// 扁平配置表：配置键 → JSON 值（数值键存 `f64`/整数，字符串键存 `String`），
    /// 由缺省值与文件覆盖合并而来，保证所有缺省键都存在。
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

    /// 按字符串读取（缺键或类型不符时回退空串）
    pub(super) fn text(&self, key: &str) -> String {
        self.values
            .get(key)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    }
}

/// 全部缺省键（含 `CONFIG_VERSION` 之外的键；用于回填判断）
pub(super) fn default_keys() -> Vec<String> {
    let mut keys: Vec<String> = NUMERIC_DEFAULTS
        .iter()
        .map(|(k, _)| k.to_string())
        .collect();
    keys.extend(STRING_DEFAULTS.iter().map(|(k, _)| k.to_string()));
    keys
}

/// 把候选值按键的类型/范围校验后返回可存值；不合法则 `None`（回落缺省）。
fn validated_value(key: &str, fallback: &Value, value: &Value) -> Option<Value> {
    if fallback.is_string() {
        return value.is_string().then(|| value.clone());
    }
    let v = value.as_f64()?;
    if !v.is_finite() {
        return None;
    }
    if RATIO_KEYS.contains(&key) {
        return (0.0..=1.0).contains(&v).then(|| json!(v));
    }
    if v < 0.0 || v.fract() != 0.0 {
        return None;
    }
    if POSITIVE_KEYS.contains(&key) && v == 0.0 {
        return None;
    }
    if key == "toolLoop" && v > 1.0 {
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
            .and_then(|v| validated_value(key, &json!(default), v))
            .unwrap_or_else(|| json!(default));
        values.insert((*key).to_string(), value);
    }
    for (key, default) in STRING_DEFAULTS {
        let value = from_file
            .and_then(|f| f.get(key))
            .and_then(|v| validated_value(key, &json!(default), v))
            .unwrap_or_else(|| json!(default));
        values.insert((*key).to_string(), value);
    }
    values.insert("configVersion".to_string(), json!(CONFIG_VERSION));
    Config { values }
}

/// loop-detect 配置文件在 agent 扩展目录下的完整路径
pub(super) fn config_path() -> PathBuf {
    agent_ext_dir().join(CONFIG_FILE)
}

/// 扩展配置目录 `agent_dir()/extensions/`
fn agent_ext_dir() -> PathBuf {
    agent_dir().join(CONFIG_SUBDIR)
}

/// loop-detect.json 读写锁（面板应用设置与读配置可能并发）。
static CONFIG_LOCK: RwLock<()> = RwLock::new(());

/// loop-detect.json 的缓存层。读/写都只在 [`CONFIG_LOCK`] 作用域内进行，
/// 锁序恒为「RwLock → 缓存 Mutex」。
static CONFIG_CACHE: ConfigCache = ConfigCache::new();

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

/// 写盘：键名一律 camelCase（如 `thinkingWindow`、`stagnationThreshold`）。
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
        // 缓存值就是落盘值（`configVersion` 也在 `values` 里）。
        CONFIG_CACHE.store(
            path,
            Value::Object(cfg.values.clone().into_iter().collect()),
        );
    }
}
