//! shrink 的配置层：`agent_dir()/extensions/shrink.json`。
//!
//! ```json
//! { "minBytes": 2048, "search": true, "log": true, "diff": true, "json": true, "keepFull": true }
//! ```
//!
//! 所有字段都可省略（缺省值见 [`ShrinkConfig::default`]）；解释不了的值退回缺省并记进
//! [`ShrinkConfig::errors`]（`/shrink` 打印），不让一个写错的字段把整份配置作废。

use super::pipeline::Kind;
use crate::core::{config_cache::ConfigCache, settings_manager::agent_dir};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
};

/// 配置文件名（位于 `agent_dir()/extensions/` 下）。
const CONFIG_FILE: &str = "shrink.json";

/// 进入压缩管道的最小字节数缺省值：小结果压了也是白压。
const DEFAULT_MIN_BYTES: usize = 2048;

/// shrink.json 的读缓存：`(path, mtime, len)` 不变就直接复用上次数值，不再读盘。
///
/// 本配置没有写入方（用户手改文件），故不需要配套的 `RwLock`。
static CONFIG_CACHE: ConfigCache = ConfigCache::new();

/// 压缩管道的配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ShrinkConfig {
    /// 进入管道的最小字节数；小于它的工具结果原样返回。
    pub min_bytes: usize,
    /// 是否启用搜索命中压缩器。
    pub search: bool,
    /// 是否启用日志压缩器。
    pub log: bool,
    /// 是否启用 diff 压缩器。
    pub diff: bool,
    /// 是否启用 JSON 表格压缩器。
    pub json: bool,
    /// 压缩后是否把原文写到临时文件并在尾部给出路径（false = 压完就丢）。
    pub keep_full: bool,
    /// 解析配置时被跳过的条目与原因（`/shrink` 打印，空表示没有问题）。
    pub errors: Vec<String>,
}

impl Default for ShrinkConfig {
    fn default() -> Self {
        Self {
            min_bytes: DEFAULT_MIN_BYTES,
            search: true,
            log: true,
            diff: true,
            json: true,
            keep_full: true,
            errors: Vec::new(),
        }
    }
}

impl ShrinkConfig {
    /// 该内容类型是否被配置放行。
    pub(super) fn allows(&self, kind: Kind) -> bool {
        match kind {
            Kind::Search => self.search,
            Kind::Log => self.log,
            Kind::Diff => self.diff,
            Kind::Json => self.json,
            Kind::Plain => false,
        }
    }
}

/// 配置文件路径：`agent_dir()/extensions/shrink.json`。
pub(super) fn config_path() -> PathBuf {
    agent_dir().join("extensions").join(CONFIG_FILE)
}

/// 读配置并解析（走缓存）；文件缺失等于全用缺省值。
pub(super) fn load() -> ShrinkConfig {
    let path = config_path();
    let value = CONFIG_CACHE.read(&path, || load_uncached(&path));
    if let Some(message) = value.get("error").and_then(Value::as_str) {
        return ShrinkConfig {
            errors: vec![message.to_string()],
            ..ShrinkConfig::default()
        };
    }
    parse(value.get("config").unwrap_or(&Value::Null))
}

/// 读盘结果编码成 JSON：成功 `{"config": <原文>}`，失败 `{"error": "…"}`——
/// [`ConfigCache`] 只缓存 `Value`，靠这个包装把解析后的内容与错误一起缓存下来。
fn load_uncached(path: &Path) -> Value {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return json!({ "config": Value::Null });
        }
        Err(err) => return json!({ "error": format!("{}: {err}", path.display()) }),
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(value) => json!({ "config": value }),
        Err(err) => json!({ "error": format!("{}: {err}", path.display()) }),
    }
}

/// 把配置 JSON 解析成 [`ShrinkConfig`]；解释不了的字段记错后保留缺省值。
fn parse(value: &Value) -> ShrinkConfig {
    let mut config = ShrinkConfig::default();
    let Some(obj) = value.as_object() else {
        if !value.is_null() {
            config.errors.push("config must be an object".to_string());
        }
        return config;
    };

    for (key, raw) in obj {
        match key.as_str() {
            "minBytes" => match raw.as_u64() {
                Some(n) => config.min_bytes = n as usize,
                None => config
                    .errors
                    .push("`minBytes` must be a non-negative integer".into()),
            },
            "search" | "log" | "diff" | "json" | "keepFull" => match raw.as_bool() {
                Some(flag) => match key.as_str() {
                    "search" => config.search = flag,
                    "log" => config.log = flag,
                    "diff" => config.diff = flag,
                    "json" => config.json = flag,
                    _ => config.keep_full = flag,
                },
                None => config.errors.push(format!("`{key}` must be a boolean")),
            },
            other => config
                .errors
                .push(format!("unknown field `{other}` (ignored)")),
        }
    }

    config
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 空配置 = 全缺省；每个字段都能单独覆盖。
    #[test]
    fn parses_overrides_and_defaults() {
        let cfg = parse(&Value::Null);
        assert_eq!(cfg, ShrinkConfig::default());
        assert!(cfg.errors.is_empty());

        let cfg = parse(&json!({ "minBytes": 512, "json": false, "keepFull": false }));
        assert_eq!(cfg.min_bytes, 512);
        assert!(!cfg.json);
        assert!(!cfg.keep_full);
        assert!(cfg.diff, "未提及的字段保持缺省");
        assert!(cfg.errors.is_empty());
    }

    /// 类型不对与未知字段都只记一条错误，其余字段照常生效。
    #[test]
    fn reports_bad_fields_and_keeps_the_rest() {
        let cfg = parse(&json!({ "minBytes": "big", "search": 1, "wat": true }));
        assert_eq!(cfg.min_bytes, DEFAULT_MIN_BYTES);
        assert!(cfg.search);
        assert_eq!(cfg.errors.len(), 3, "{:?}", cfg.errors);
        assert!(cfg.errors.iter().any(|e| e.contains("unknown field `wat`")));
    }

    /// 非对象整份记一条错误并退回缺省。
    #[test]
    fn rejects_non_object() {
        assert_eq!(parse(&json!(1)).errors.len(), 1);
        assert!(parse(&Value::Null).errors.is_empty());
    }

    /// 按类型放行：Plain 恒不放行；关掉的类型不放行。
    #[test]
    fn gates_by_kind() {
        let mut cfg = ShrinkConfig::default();
        assert!(cfg.allows(Kind::Search));
        assert!(!cfg.allows(Kind::Plain));
        cfg.diff = false;
        assert!(!cfg.allows(Kind::Diff));
        assert!(cfg.allows(Kind::Log));
    }
}
