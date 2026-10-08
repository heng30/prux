//! `web-search.json` 配置（移植自 pi-web-access `utils.ts` / `feature-config.ts`）。
//!
//! 配置位于 `agent_dir()/extensions/web-search.json`（可用 `PRUX_AGENT_DIR` 覆盖）。
//! 其余键名、语义保持一致，未知键忽略、每个字段可选。
//!
//! 凭据值支持 `$ENV_VAR` 间接引用（见 [`resolve_credential`]），
//! 「配置值优先、环境变量兜底」一致；浏览器 cookie / OAuth 凭据源不在本阶段移植范围。

use crate::core::{config_cache::ConfigCache, settings_manager::agent_dir};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::RwLock,
    time::Duration,
};

/// web-search.json 读写锁（`/reload` 清理与读配置可能并发）。
static CONFIG_LOCK: RwLock<()> = RwLock::new(());

/// web-search.json 的缓存层。读/写都只在 [`CONFIG_LOCK`] 作用域内进行，
/// 锁序恒为「RwLock → 缓存 Mutex」。
static CONFIG_CACHE: ConfigCache = ConfigCache::new();

/// 配置文件路径：`agent_dir()/extensions/web-search.json`
pub fn config_path() -> PathBuf {
    agent_dir().join("extensions").join("web-search.json")
}

/// 默认配置模板：只列出已实现的键，便于用户对照增删。
///
/// 凭据写 `$ENV_VAR` 表示从对应环境变量读取；留空则回退到同名环境变量。
/// 顶层的 `_comment` 是说明字段，反序列化时忽略（未知键一律忽略）。
const DEFAULT_CONFIG_TEMPLATE: &str = r#"{
  "_comment": "web-access config (all keys optional). Credentials accept a literal value or \"$ENV_VAR\"; empty falls back to the matching env var (BRAVE_API_KEY / EXA_API_KEY / TAVILY_API_KEY / JINA_API_KEY / SERPER_API_KEY / PERPLEXITY_API_KEY).",
  "provider": "auto",
  "braveApiKey": "$BRAVE_API_KEY",
  "braveBaseUrl": "https://api.search.brave.com/res/v1",
  "exaApiKey": "$EXA_API_KEY",
  "exaBaseUrl": "https://api.exa.ai",
  "tavilyApiKey": "$TAVILY_API_KEY",
  "tavilyBaseUrl": "https://api.tavily.com",
  "jinaApiKey": "$JINA_API_KEY",
  "serperApiKey": "$SERPER_API_KEY",
  "perplexityApiKey": "$PERPLEXITY_API_KEY",
  "searxngBaseUrl": "",
  "searxngHeaders": {},
  "userAgent": "",
  "proxy": "",
  "timeout": 30,
  "maxInlineContentChars": 30000,
  "allowPrivateHosts": false,
  "ssrfAllowRanges": [],
  "searchRouting": {
    "providers": []
  },
  "fetch": {
    "timeout": 30
  },
  "tools": {
    "webSearch": { "enabled": true },
    "fetchContent": { "enabled": true },
    "getSearchContent": { "enabled": true }
  },
  "webSearch": { "enabled": true }
}
"#;

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
    _ = std::fs::write(&path, DEFAULT_CONFIG_TEMPLATE);
}

/// 反序列化「字符串或标量」为 Option<String>：非字符串标量转成字符串，对象/数组忽略。
fn opt_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    Ok(value.and_then(value_to_string))
}

/// 把标量 JSON 值转成字符串：非空字符串原样、数字/布尔转字面量，
/// 对象/数组/空字符串返回 None（这些位置只接受标量）。
fn value_to_string(value: Value) -> Option<String> {
    match value {
        Value::String(s) if !s.trim().is_empty() => Some(s),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// `web-search.json` 顶层结构（只声明本阶段消费的键）。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Config {
    /// 默认搜索 provider（省略/`auto` 时走路由）
    #[serde(deserialize_with = "opt_string")]
    pub provider: Option<String>,
    /// Brave Search API 密钥
    #[serde(deserialize_with = "opt_string")]
    pub brave_api_key: Option<String>,
    /// Brave Search API 端点覆盖
    #[serde(deserialize_with = "opt_string")]
    pub brave_base_url: Option<String>,
    /// Exa API 密钥
    #[serde(deserialize_with = "opt_string")]
    pub exa_api_key: Option<String>,
    /// Exa API 端点覆盖
    #[serde(deserialize_with = "opt_string")]
    pub exa_base_url: Option<String>,
    /// Tavily API 密钥
    #[serde(deserialize_with = "opt_string")]
    pub tavily_api_key: Option<String>,
    /// Tavily API 端点覆盖
    #[serde(deserialize_with = "opt_string")]
    pub tavily_base_url: Option<String>,
    /// Jina Reader API 密钥（用于取正文）
    #[serde(deserialize_with = "opt_string")]
    pub jina_api_key: Option<String>,
    /// SearXNG 实例基址（自托管，无需密钥）
    #[serde(deserialize_with = "opt_string")]
    pub searxng_base_url: Option<String>,
    /// SearXNG 请求附加头（如鉴权/伪造浏览器头）
    pub searxng_headers: Option<BTreeMap<String, String>>,
    /// Serper（Google）API 密钥
    #[serde(deserialize_with = "opt_string")]
    pub serper_api_key: Option<String>,
    /// Perplexity API 密钥
    #[serde(deserialize_with = "opt_string")]
    pub perplexity_api_key: Option<String>,
    /// 搜索路由策略（provider 顺序、能否用当前模型、回退条件）
    pub search_routing: SearchRouting,
    /// 取正文相关设置（超时、作答模型）
    pub fetch: FetchSettings,
    /// 工具级开关
    pub tools: ToolToggles,
    /// 斜杠命令级开关
    pub commands: CommandToggles,
    /// 单次内联返回的最大字符数（默认 30000）
    pub max_inline_content_chars: Option<usize>,
    /// `none` / `summary-review` / `auto-summary`（本阶段只支持 none 语义）
    #[serde(deserialize_with = "opt_string")]
    pub workflow: Option<String>,
    /// HTTP(S) 代理地址（等价 `HTTPS_PROXY`）
    #[serde(deserialize_with = "opt_string")]
    pub proxy: Option<String>,
    /// 单次 HTTP 超时秒数（默认 30）
    pub timeout: Option<u64>,
    /// 顶层兼容开关：`{ "webSearch": { "enabled": false } }`
    pub web_search: Option<Toggle>,
    /// 搜索时附加的 User-Agent 覆盖（非原版键，便于内网网关）
    #[serde(deserialize_with = "opt_string")]
    pub user_agent: Option<String>,
    /// 允许访问私有/回环地址（默认 false；SSRF 防护）
    pub allow_private_hosts: Option<bool>,
    /// SSRF 白名单网段（CIDR 字符串）
    pub ssrf_allow_ranges: Vec<String>,
}

/// 搜索 provider 的路由顺序与触发回退的错误类别。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SearchRouting {
    /// 路由 provider 顺序（数组或单个字符串）
    #[serde(deserialize_with = "string_list")]
    pub providers: Vec<String>,
    /// 触发回退的错误类别（transient/quota/network/invalid-response）
    pub fallback_on: Vec<String>,
}

/// 抓取正文的超时等设置，可覆盖顶层 `timeout`。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct FetchSettings {
    /// 取正文超时秒数（覆盖顶层 `timeout`）
    pub timeout: Option<u64>,
}

/// 工具开关：`{ "tools": { "webSearch": { "enabled": false } } }`
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ToolToggles {
    /// 联网搜索工具
    pub web_search: Option<Toggle>,
    /// 来源核查工具
    pub source_check: Option<Toggle>,
    /// 抓取网页正文工具
    pub fetch_content: Option<Toggle>,
    /// 回读取回搜索内容工具
    pub get_search_content: Option<Toggle>,
}

/// 命令开关：`{ "commands": { "websearch": { "enabled": false } } }`
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CommandToggles {
    /// `/websearch` 命令
    pub websearch: Option<Toggle>,
    /// `/curator` 命令（搜索结果整理）
    pub curator: Option<Toggle>,
}

/// 单个工具/命令的启停开关，未配置视为启用。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Toggle {
    /// 开关值；`None`（未配置）按启用处理，`Some(false)` 才关闭
    pub enabled: Option<bool>,
}

impl Toggle {
    /// 未配置视为启用；显式 `false` 才关闭。
    pub fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }
}

/// 反序列化「字符串或字符串数组」为 `Vec<String>`：单个非空字符串包成一项，
/// 数组则过滤掉非字符串与空白项；其它类型得到空列表。
fn string_list<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    Ok(match value {
        Some(Value::String(s)) if !s.trim().is_empty() => vec![s],
        Some(Value::Array(items)) => items
            .into_iter()
            .filter_map(|item| match item {
                Value::String(s) if !s.trim().is_empty() => Some(s),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    })
}

/// 配置加载结果：成功得到配置，失败保留解析错误（原版对损坏 JSON 抛错）。
#[derive(Debug, Clone)]
pub struct LoadedConfig {
    /// 解析得到的配置（解析失败时为缺省值）
    pub config: Config,
    /// 读取/解析错误（成功时为 `None`）
    pub error: Option<String>,
}

impl LoadedConfig {
    /// 工具开关是否启用（顶层 `webSearch.enabled` 与 `tools.webSearch.enabled` 任一
    /// 为 false 即关闭；`sourceCheck`/`fetchContent`/`getSearchContent` 只看 tools）。
    pub fn tool_enabled(&self, name: FeatureTool) -> bool {
        let cfg = &self.config;
        match name {
            FeatureTool::WebSearch => {
                cfg.tools
                    .web_search
                    .as_ref()
                    .map(Toggle::is_enabled)
                    .unwrap_or(true)
                    && cfg
                        .web_search
                        .as_ref()
                        .map(Toggle::is_enabled)
                        .unwrap_or(true)
            }
            FeatureTool::SourceCheck => cfg
                .tools
                .source_check
                .as_ref()
                .map(Toggle::is_enabled)
                .unwrap_or(true),
            FeatureTool::FetchContent => cfg
                .tools
                .fetch_content
                .as_ref()
                .map(Toggle::is_enabled)
                .unwrap_or(true),
            FeatureTool::GetSearchContent => cfg
                .tools
                .get_search_content
                .as_ref()
                .map(Toggle::is_enabled)
                .unwrap_or(true),
        }
    }
}

/// 可独立开关的联网功能工具，供配置查询启用状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeatureTool {
    /// 联网搜索
    WebSearch,
    /// 来源核查
    #[allow(dead_code)]
    SourceCheck,
    /// 抓取网页正文
    FetchContent,
    /// 回读取回搜索内容
    GetSearchContent,
}

/// 读取配置（带进程内缓存，命中免读盘/解析；解析错误也一并缓存）。
pub fn load() -> LoadedConfig {
    let path = config_path();
    let _guard = CONFIG_LOCK.read().unwrap_or_else(|e| e.into_inner());
    let value = CONFIG_CACHE.read(&path, || load_uncached(&path));
    loaded_config(&value)
}

/// 清除配置缓存（文件变更或 /reload 后调用）
pub fn invalidate() {
    let _guard = CONFIG_LOCK.write().unwrap_or_else(|e| e.into_inner());
    CONFIG_CACHE.invalidate();
}

/// 读盘结果编码为 JSON：成功 `{"config": {...}}`，失败 `{"error": "..."}`。
/// [`ConfigCache`] 只缓存 `Value`，靠这个包装同时缓存配置对象与解析错误。
fn load_uncached(path: &Path) -> Value {
    if !path.exists() {
        return json!({ "config": Value::Null });
    }

    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<Config>(&text) {
            Ok(config) => json!({ "config": config }),
            Err(err) => json!({ "error": format!("Failed to parse {}: {err}", path.display()) }),
        },
        Err(err) => json!({ "error": format!("Failed to read {}: {err}", path.display()) }),
    }
}

/// 解码 [`load_uncached`] 的缓存值。
fn loaded_config(value: &Value) -> LoadedConfig {
    if let Some(error) = value.get("error").and_then(|e| e.as_str()) {
        return LoadedConfig {
            config: Config::default(),
            error: Some(error.to_string()),
        };
    }

    let config = value
        .get("config")
        .filter(|c| c.is_object())
        .and_then(|c| serde_json::from_value::<Config>(c.clone()).ok())
        .unwrap_or_default();

    LoadedConfig {
        config,
        error: None,
    }
}

/// 解析凭据：配置值优先（支持 `$ENV` 间接引用），否则回退到进程环境变量。返回 `None` 表示不可用。
pub fn resolve_credential(configured: Option<&str>, env_name: &str) -> Option<String> {
    if let Some(raw) = configured {
        let raw = raw.trim();
        if let Some(name) = raw.strip_prefix('$') {
            let name = name.trim();
            if !name.is_empty()
                && let Ok(value) = std::env::var(name)
                && !value.trim().is_empty()
            {
                return Some(value);
            }
            return None;
        }

        if !raw.is_empty() {
            return Some(raw.to_string());
        }
    }

    std::env::var(env_name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

/// 有效内联内容上限（默认 30000）
pub fn max_inline_content_chars(config: &Config) -> usize {
    config
        .max_inline_content_chars
        .filter(|n| *n > 0)
        .unwrap_or(30_000)
}

/// 有效 HTTP 超时（fetch.timeout 优先，否则顶层 timeout，默认 30s）
pub fn http_timeout(config: &Config) -> Duration {
    let secs = config
        .fetch
        .timeout
        .or(config.timeout)
        .filter(|n| *n > 0)
        .unwrap_or(30);
    Duration::from_secs(secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_camel_case_and_env_credentials() {
        let cfg: Config = serde_json::from_str(
            r#"{
                "provider": "brave",
                "braveApiKey": "$TEST_BRAVE_KEY",
                "maxInlineContentChars": 1234,
                "tools": { "sourceCheck": { "enabled": false } },
                "searchRouting": { "providers": ["brave", "tavily"] },
                "webSearch": { "enabled": true }
            }"#,
        )
        .unwrap();
        assert_eq!(cfg.provider.as_deref(), Some("brave"));
        assert_eq!(cfg.max_inline_content_chars, Some(1234));
        assert_eq!(cfg.search_routing.providers, vec!["brave", "tavily"]);
        let loaded = LoadedConfig {
            config: cfg,
            error: None,
        };
        assert!(!loaded.tool_enabled(FeatureTool::SourceCheck));
        assert!(loaded.tool_enabled(FeatureTool::WebSearch));
        assert_eq!(max_inline_content_chars(&loaded.config), 1234);
    }

    #[test]
    fn credential_env_indirection() {
        // 使用唯一变量名，避免与并行测试互相干扰
        unsafe { std::env::set_var("PRUX_TEST_WEB_KEY", "from-env") };
        assert_eq!(
            resolve_credential(Some("$PRUX_TEST_WEB_KEY"), "UNUSED"),
            Some("from-env".to_string())
        );
        assert_eq!(
            resolve_credential(Some("literal"), "UNUSED"),
            Some("literal".to_string())
        );
        assert_eq!(
            resolve_credential(Some("$PRUX_TEST_WEB_MISSING"), "UNUSED"),
            None
        );
        assert_eq!(
            resolve_credential(None, "PRUX_TEST_WEB_KEY"),
            Some("from-env".to_string())
        );
        unsafe { std::env::remove_var("PRUX_TEST_WEB_KEY") };
    }

    #[test]
    fn corrupted_config_reports_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("web-search.json");
        std::fs::write(&path, "{ not json").unwrap();
        let loaded = loaded_config(&load_uncached(&path));
        assert!(loaded.error.is_some());
    }

    /// 缓存以 `(path, mtime, len)` 为有效键：外部改写/删除后，`load` 必须看到新值。
    #[test]
    fn cache_reflects_external_edits() {
        let _g = crate::test_support::AgentDirGuard::temp();
        let path = config_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();

        std::fs::write(&path, r#"{"provider":"brave"}"#).unwrap();
        assert_eq!(load().config.provider.as_deref(), Some("brave"));

        std::fs::write(&path, r#"{"provider":"tavily"}"#).unwrap();
        assert_eq!(load().config.provider.as_deref(), Some("tavily"));

        std::fs::remove_file(&path).unwrap();
        let loaded = load();
        assert!(loaded.error.is_none());
        assert!(loaded.config.provider.is_none());
    }

    #[test]
    fn ensure_config_file_writes_parseable_template_when_missing() {
        let _g = crate::test_support::AgentDirGuard::temp();
        assert!(!config_path().exists());
        ensure_config_file();
        assert!(config_path().exists());

        let loaded = load();
        assert!(loaded.error.is_none(), "模板必须可解析：{:?}", loaded.error);
        // 默认模板下三个工具都启用，且不含未实现键。
        assert!(loaded.tool_enabled(FeatureTool::WebSearch));
        assert!(loaded.tool_enabled(FeatureTool::FetchContent));
        assert!(loaded.tool_enabled(FeatureTool::GetSearchContent));
    }

    #[test]
    fn ensure_config_file_never_overwrites_existing() {
        let _g = crate::test_support::AgentDirGuard::temp();
        let path = config_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{\"provider\":\"brave\"}").unwrap();
        ensure_config_file();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\"provider\":\"brave\"}"
        );
    }
}
