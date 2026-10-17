//! `browser.json` 配置：CDP 端点、候选浏览器与输出上限（`agent_dir()/extensions/browser.json`）。
//!
//! 全部键可选、未知键忽略；文件不存在时用 [`Config::default`]。
//! 只有**回环**主机能通过校验：CDP 调试端点没有鉴权，连到别的机器等于把浏览器交出去。

use super::launch;
use crate::core::{config_cache::ConfigCache, settings_manager::agent_dir};
use crate::utils::net;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::RwLock,
    time::Duration,
};

/// `browser.json` 的读写锁（`/reload` 清理与读配置可能并发）。
static CONFIG_LOCK: RwLock<()> = RwLock::new(());

/// `browser.json` 的缓存层。读/写都只在 [`CONFIG_LOCK`] 作用域内进行，
/// 锁序恒为「RwLock → 缓存 Mutex」。
static CONFIG_CACHE: ConfigCache = ConfigCache::new();

/// 默认调试主机：只连本机。
pub const DEFAULT_HOST: &str = "127.0.0.1";

/// 默认调试端口：Chrome / Chromium / Edge / Brave 的 `--remote-debugging-port` 惯例值。
pub const DEFAULT_PORT: u16 = 9222;

/// 默认单条 CDP 命令的超时秒数。
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// 默认 `snapshot` 返回文本的字符上限。
pub const DEFAULT_MAX_SNAPSHOT_CHARS: usize = 20_000;

/// 默认 `get_content` 返回文本的字符上限。
pub const DEFAULT_MAX_CONTENT_CHARS: usize = 50_000;

/// 默认 `snapshot` 收集的可交互元素条数上限。
pub const DEFAULT_MAX_ELEMENTS: usize = 200;

/// 超时秒数的允许区间（下界 1s，上界 10min）。
const TIMEOUT_RANGE: (u64, u64) = (1, 600);

/// `snapshot` 字符上限的允许区间。
const SNAPSHOT_CHARS_RANGE: (usize, usize) = (500, 1_000_000);

/// `get_content` 字符上限的允许区间。
const CONTENT_CHARS_RANGE: (usize, usize) = (500, 2_000_000);

/// `snapshot` 元素条数上限的允许区间。
const MAX_ELEMENTS_RANGE: (usize, usize) = (1, 2000);

/// 配置文件路径：`agent_dir()/extensions/browser.json`。
pub fn config_path() -> PathBuf {
    agent_dir().join("extensions").join("browser.json")
}

/// 默认配置模板（首次启用时落盘，便于用户对照增删）。
///
/// `browsers` 按当前平台的内置候选列表展开（JSON 字符串由 Rust 的 `Debug` 转义，
/// 恰好与 JSON 的转义规则一致）。
fn default_template() -> String {
    let browsers = launch::default_browsers()
        .iter()
        .map(|program| format!("    {program:?}"))
        .collect::<Vec<_>>()
        .join(",\n");
    format!(
        r#"{{
  "_comment": "browser extension config (all keys optional). The CDP endpoint has no authentication, so host must be a loopback address.",
  "host": "127.0.0.1",
  "port": 9222,
  "timeout": 30,
  "browsers": [
{browsers}
  ],
  "maxSnapshotChars": 20000,
  "maxContentChars": 50000,
  "maxElements": 200
}}
"#
    )
}

/// 首次启用时落盘默认配置模板：文件缺失才写入（建目录），已存在则绝不覆盖。
pub fn ensure_config_file() {
    let path = config_path();
    if path.exists() {
        return;
    }

    if let Some(parent) = path.parent()
        && std::fs::create_dir_all(parent).is_err()
    {
        return;
    }
    _ = std::fs::write(&path, default_template());
}

/// 生效配置（已带默认值与区间钳制）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// CDP 调试主机，必须是回环地址（`localhost` / `127.0.0.1` / `::1`）。
    pub host: String,
    /// CDP 调试端口（浏览器启动时 `--remote-debugging-port=<port>`）。
    pub port: u16,
    /// 单条 CDP 命令（含 WebSocket 握手与 HTTP 发现）的超时，也是等自动启动的浏览器就绪的上限。
    pub timeout: Duration,
    /// 自动启动的候选浏览器启动命令，按顺序尝试；空数组 = 只连已有端口，不启动任何浏览器。
    pub browsers: Vec<String>,
    /// `snapshot` 返回文本的字符上限。
    pub max_snapshot_chars: usize,
    /// `get_content` 返回文本的字符上限。
    pub max_content_chars: usize,
    /// `snapshot` 收集的可交互元素条数上限。
    pub max_elements: usize,
}

impl Default for Config {
    /// 全部取默认值（127.0.0.1:9222、30s、2 万字符快照、5 万字符正文、200 个元素、平台默认候选浏览器）。
    fn default() -> Self {
        Self {
            host: DEFAULT_HOST.to_string(),
            port: DEFAULT_PORT,
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            browsers: launch::default_browsers(),
            max_snapshot_chars: DEFAULT_MAX_SNAPSHOT_CHARS,
            max_content_chars: DEFAULT_MAX_CONTENT_CHARS,
            max_elements: DEFAULT_MAX_ELEMENTS,
        }
    }
}

impl Config {
    /// 校验取值：非回环主机、`port = 0`、越界的上限都直接报错（不静默钳制，避免用户以为改生效了）。
    pub fn validate(&self) -> Result<(), String> {
        if !net::is_loopback_host(&self.host) {
            return Err(format!(
                "browser: config `host` must be a loopback address (localhost / 127.0.0.1 / ::1), got `{}`; the CDP endpoint has no authentication",
                self.host
            ));
        }

        if self.port == 0 {
            return Err("browser: config `port` must be 1-65535".to_string());
        }

        let secs = self.timeout.as_secs();
        if !(TIMEOUT_RANGE.0..=TIMEOUT_RANGE.1).contains(&secs) {
            return Err(format!(
                "browser: config `timeout` must be {}..={} seconds, got {secs}",
                TIMEOUT_RANGE.0, TIMEOUT_RANGE.1
            ));
        }

        if let Some(index) = self.browsers.iter().position(|p| p.trim().is_empty()) {
            return Err(format!(
                "browser: config `browsers[{index}]` is empty; remove the entry to stop auto-starting that candidate"
            ));
        }

        for (key, value, range) in [
            (
                "maxSnapshotChars",
                self.max_snapshot_chars,
                SNAPSHOT_CHARS_RANGE,
            ),
            (
                "maxContentChars",
                self.max_content_chars,
                CONTENT_CHARS_RANGE,
            ),
            ("maxElements", self.max_elements, MAX_ELEMENTS_RANGE),
        ] {
            if !(range.0..=range.1).contains(&value) {
                return Err(format!(
                    "browser: config `{key}` must be {}..={}, got {value}",
                    range.0, range.1
                ));
            }
        }
        Ok(())
    }
}

/// 读盘结构：全部键可选。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
struct FileConfig {
    /// CDP 调试主机
    host: Option<String>,
    /// CDP 调试端口
    port: Option<u16>,
    /// 单条 CDP 命令超时秒数
    timeout: Option<u64>,
    /// 自动启动的候选浏览器启动命令（按顺序尝试）
    browsers: Option<Vec<String>>,
    /// `snapshot` 返回文本的字符上限
    max_snapshot_chars: Option<usize>,
    /// `get_content` 返回文本的字符上限
    max_content_chars: Option<usize>,
    /// `snapshot` 收集的元素条数上限
    max_elements: Option<usize>,
}

/// 一次配置读取的结果：生效配置 + 读盘/校验错误（有错误时配置不可用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loaded {
    /// 生效配置（出错时是默认值，仅用于展示）。
    pub config: Config,
    /// 读盘或校验失败的原因；`None` = 配置可用。
    pub error: Option<String>,
}

/// 读配置（缓存命中不读盘）；解析或校验失败时 `error` 为 `Some`。
pub fn load() -> Loaded {
    let path = config_path();
    let _guard = CONFIG_LOCK.read().unwrap_or_else(|e| e.into_inner());
    let value = CONFIG_CACHE.read(&path, || load_uncached(&path));
    loaded_config(&value)
}

/// 清除配置缓存（`/reload` 或启用态变化后调用）。
pub fn invalidate() {
    let _guard = CONFIG_LOCK.write().unwrap_or_else(|e| e.into_inner());
    CONFIG_CACHE.invalidate();
}

/// 读盘结果编码为 JSON：成功 `{"config": {...}}`，失败 `{"error": "..."}`。
///
/// [`ConfigCache`] 只缓存 `Value`，靠这个包装同时缓存配置对象与解析错误。
fn load_uncached(path: &Path) -> Value {
    if !path.exists() {
        return json!({ "config": Value::Null });
    }

    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<FileConfig>(&text) {
            Ok(file) => json!({ "config": file }),
            Err(err) => json!({ "error": format!("Failed to parse {}: {err}", path.display()) }),
        },
        Err(err) => json!({ "error": format!("Failed to read {}: {err}", path.display()) }),
    }
}

/// 把读盘结果（或读盘错误）转成 [`Loaded`]。
fn loaded_config(value: &Value) -> Loaded {
    if let Some(error) = value.get("error").and_then(Value::as_str) {
        return Loaded {
            config: Config::default(),
            error: Some(format!("browser: {error}")),
        };
    }

    let Some(config) = value.get("config").filter(|v| !v.is_null()) else {
        return Loaded {
            config: Config::default(),
            error: None,
        };
    };

    let Ok(file) = serde_json::from_value::<FileConfig>(config.clone()) else {
        return Loaded {
            config: Config::default(),
            error: Some("browser: config could not be decoded".to_string()),
        };
    };

    let defaults = Config::default();
    let config = Config {
        host: file
            .host
            .filter(|h| !h.trim().is_empty())
            .unwrap_or(defaults.host),
        port: file.port.unwrap_or(defaults.port),
        timeout: Duration::from_secs(file.timeout.unwrap_or(DEFAULT_TIMEOUT_SECS)),
        browsers: match file.browsers {
            Some(browsers) => browsers
                .into_iter()
                .map(|program| program.trim().to_string())
                .collect(),
            None => defaults.browsers,
        },
        max_snapshot_chars: file
            .max_snapshot_chars
            .unwrap_or(defaults.max_snapshot_chars),
        max_content_chars: file.max_content_chars.unwrap_or(defaults.max_content_chars),
        max_elements: file.max_elements.unwrap_or(defaults.max_elements),
    };

    match config.validate() {
        Ok(()) => Loaded {
            config,
            error: None,
        },
        Err(error) => Loaded {
            config: Config::default(),
            error: Some(error),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::AgentDirGuard;

    /// 文件不存在时用默认值，且不报错。
    #[test]
    fn missing_file_falls_back_to_defaults() {
        let _dir = AgentDirGuard::temp();
        let loaded = load();
        assert_eq!(loaded.error, None);
        assert_eq!(loaded.config, Config::default());
    }

    /// 部分键覆盖其余键保持默认（含 `browsers`：没写就用内置候选列表）。
    #[test]
    fn partial_file_overrides_only_given_keys() {
        let _dir = AgentDirGuard::temp();
        ensure_config_file();
        std::fs::write(config_path(), r#"{ "port": 9333, "maxElements": 7 }"#).unwrap();
        invalidate();

        let loaded = load();
        assert_eq!(loaded.error, None);
        assert_eq!(loaded.config.port, 9333);
        assert_eq!(loaded.config.max_elements, 7);
        assert_eq!(loaded.config.host, DEFAULT_HOST);
        assert_eq!(loaded.config.timeout, Duration::from_secs(30));
        assert_eq!(loaded.config.browsers, launch::default_browsers());
    }

    /// `browsers` 给了就以配置为准（空数组 = 不自动启动），表项前后空白被去掉。
    #[test]
    fn browsers_key_replaces_the_builtin_list() {
        let _dir = AgentDirGuard::temp();
        ensure_config_file();
        std::fs::write(
            config_path(),
            r#"{ "browsers": [" my-chrome ", "/opt/brave/brave"] }"#,
        )
        .unwrap();
        invalidate();
        assert_eq!(
            load().config.browsers,
            vec!["my-chrome".to_string(), "/opt/brave/brave".to_string()]
        );

        std::fs::write(config_path(), r#"{ "browsers": [] }"#).unwrap();
        invalidate();
        assert!(load().config.browsers.is_empty());
    }

    /// 空的 `browsers` 条目直接报错（静默忽略会让用户以为它生效了）。
    #[test]
    fn empty_browser_entries_are_rejected() {
        let config = Config {
            browsers: vec!["   ".to_string()],
            ..Config::default()
        };
        assert!(config.validate().unwrap_err().contains("browsers[0]"));
    }

    /// 模板文件本身必须是合法配置（否则首次启用就把用户带沟里），且列出平台的默认候选浏览器。
    #[test]
    fn default_template_is_valid() {
        let _dir = AgentDirGuard::temp();
        ensure_config_file();
        let text = std::fs::read_to_string(config_path()).unwrap();
        let file: FileConfig = serde_json::from_str(&text).unwrap();
        let browsers = file.browsers.clone().expect("模板应列出默认候选浏览器");
        assert_eq!(browsers, launch::default_browsers());
        let config = Config {
            host: file.host.unwrap(),
            port: file.port.unwrap(),
            timeout: Duration::from_secs(file.timeout.unwrap()),
            browsers,
            max_snapshot_chars: file.max_snapshot_chars.unwrap(),
            max_content_chars: file.max_content_chars.unwrap(),
            max_elements: file.max_elements.unwrap(),
        };
        assert_eq!(config.validate(), Ok(()));
    }

    /// 非回环主机被拒绝（CDP 端点无鉴权）。
    #[test]
    fn non_loopback_host_is_rejected() {
        let config = Config {
            host: "10.0.0.5".to_string(),
            ..Config::default()
        };
        assert!(config.validate().unwrap_err().contains("loopback"));
    }

    /// 越界取值直接报错，而不是静默钳制。
    #[test]
    fn out_of_range_values_are_rejected() {
        let port = Config {
            port: 0,
            ..Config::default()
        };
        assert!(port.validate().unwrap_err().contains("port"));

        let timeout = Config {
            timeout: Duration::from_secs(0),
            ..Config::default()
        };
        assert!(timeout.validate().unwrap_err().contains("timeout"));

        let elements = Config {
            max_elements: 0,
            ..Config::default()
        };
        assert!(elements.validate().unwrap_err().contains("maxElements"));
    }

    /// 坏 JSON 报错并回落到默认配置。
    #[test]
    fn malformed_file_reports_an_error() {
        let _dir = AgentDirGuard::temp();
        ensure_config_file();
        std::fs::write(config_path(), "{ not json").unwrap();
        invalidate();

        let loaded = load();
        assert!(loaded.error.unwrap().contains("Failed to parse"));
        assert_eq!(loaded.config, Config::default());
    }
}
