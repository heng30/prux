//! `proxy` 网关配置：`agent_dir()/extensions/proxy.json`。
//!
//! 键：
//! - `listen`：监听地址 `host:port`（缺省 = 不起服务）；
//! - `provider`：目标 provider（缺省 = 未配置）；
//! - `apiKey`：入站 Bearer token（缺省/空 = 不鉴权；`PRUX_PROXY_TOKEN` 环境变量优先）。
//!
//! 键名统一 camelCase。
//!
//! 文件缺失或损坏时按「未配置」处理，**不覆盖**用户手写内容（仅在扩展写配置时落盘）。

use crate::core::{config_cache::ConfigCache, settings_manager::agent_dir};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{path::PathBuf, sync::RwLock};

/// 扩展配置子目录（与 loop-detect / web-search 一致）
const CONFIG_SUBDIR: &str = "extensions";

/// 配置文件名
const CONFIG_FILE: &str = "proxy.json";

/// 入站 token 的环境变量名（优先于配置文件 `apiKey`）
pub const TOKEN_ENV: &str = "PRUX_PROXY_TOKEN";

/// proxy.json 读写锁（面板保存与读配置可能并发）。
static CONFIG_LOCK: RwLock<()> = RwLock::new(());

/// proxy.json 的缓存层。读/写都只在 [`CONFIG_LOCK`] 作用域内进行，锁序恒为「RwLock → 缓存 Mutex」。
static CONFIG_CACHE: ConfigCache = ConfigCache::new();

/// `proxy` 网关配置（`agent_dir()/extensions/proxy.json`）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyConfig {
    /// 监听地址 `host:port`；`None` = 不起服务
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listen: Option<String>,
    /// 目标 provider；`None` = 未配置
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// 入站 Bearer token；`None`/空 = 不鉴权（`PRUX_PROXY_TOKEN` 环境变量优先）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

/// 配置文件路径：`agent_dir()/extensions/proxy.json`
pub fn config_path() -> PathBuf {
    agent_dir().join(CONFIG_SUBDIR).join(CONFIG_FILE)
}

/// 读配置：缺失/损坏 → 默认（不写盘、不覆盖）。走 [`CONFIG_CACHE`]。
pub fn load() -> ProxyConfig {
    let path = config_path();
    let _guard = CONFIG_LOCK.read().unwrap_or_else(|e| e.into_inner());
    let value = CONFIG_CACHE.read(&path, || {
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .unwrap_or(Value::Null)
    });
    serde_json::from_value(value).unwrap_or_default()
}

/// 写配置（建目录；失败静默——配置写不进去不该打断 TUI）。
/// 落盘成功后回填缓存（取写盘后的元数据）。
pub fn save(cfg: &ProxyConfig) {
    let path = config_path();
    let _guard = CONFIG_LOCK.write().unwrap_or_else(|e| e.into_inner());

    if let Some(parent) = path.parent() {
        _ = std::fs::create_dir_all(parent);
    }

    if let Ok(text) = serde_json::to_string_pretty(cfg)
        && std::fs::write(&path, format!("{text}\n")).is_ok()
        && let Ok(value) = serde_json::to_value(cfg)
    {
        CONFIG_CACHE.store(&path, value);
    }
}

/// 入站 token 的来源（`PRUX_PROXY_TOKEN` 优先于配置文件 `apiKey`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenSource {
    /// 环境变量 `PRUX_PROXY_TOKEN`
    Env,
    /// 配置文件 `apiKey`
    Config,
}

/// 入站 token 及其来源；空串视同未设置。
pub fn token_with_source(cfg: &ProxyConfig) -> Option<(String, TokenSource)> {
    let env = std::env::var(TOKEN_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty());
    match env {
        Some(token) => Some((token, TokenSource::Env)),
        None => cfg
            .api_key
            .clone()
            .filter(|v| !v.trim().is_empty())
            .map(|token| (token, TokenSource::Config)),
    }
}

/// 入站 token：`PRUX_PROXY_TOKEN` 优先，其次 `api_key`；空串视同未设置。
pub fn token(cfg: &ProxyConfig) -> Option<String> {
    token_with_source(cfg).map(|(token, _)| token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{AgentDirGuard, env_key_lock};

    #[test]
    fn load_missing_or_corrupt_is_default_and_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let _g = AgentDirGuard::set(dir.path());
        assert_eq!(load(), ProxyConfig::default());

        // 损坏文件不覆盖（用户手写内容保留）
        let path = dir.path().join("extensions/proxy.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{ not json").unwrap();
        assert_eq!(load(), ProxyConfig::default());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
    }

    #[test]
    fn save_roundtrip_uses_camel_case_api_key() {
        let dir = tempfile::tempdir().unwrap();
        let _g = AgentDirGuard::set(dir.path());
        let cfg = ProxyConfig {
            listen: Some("127.0.0.1:8765".into()),
            provider: Some("deepseek".into()),
            api_key: Some("secret".into()),
        };
        save(&cfg);
        let text = std::fs::read_to_string(config_path()).unwrap();
        assert!(text.contains("\"apiKey\": \"secret\""), "{text}");
        assert!(!text.contains("api_key"), "不应再写 `api_key`：{text}");
        assert_eq!(load(), cfg);
    }

    /// 旧键 `api_key` 不认（不兼容），且被忽略而非报错——文件按「部分未配置」处理。
    #[test]
    fn legacy_snake_case_api_key_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let _g = AgentDirGuard::set(dir.path());
        let path = dir.path().join("extensions/proxy.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();

        std::fs::write(&path, r#"{"listen":"127.0.0.1:1111","api_key":"old"}"#).unwrap();
        let cfg = load();
        assert_eq!(cfg.listen.as_deref(), Some("127.0.0.1:1111"));
        assert_eq!(cfg.api_key, None);
    }

    #[test]
    fn load_reflects_external_edits() {
        let dir = tempfile::tempdir().unwrap();
        let _g = AgentDirGuard::set(dir.path());
        let path = dir.path().join("extensions/proxy.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();

        std::fs::write(&path, r#"{"listen":"127.0.0.1:1111"}"#).unwrap();
        assert_eq!(load().listen.as_deref(), Some("127.0.0.1:1111"));

        // 外部改写（长度也不同）→ 缓存失效，读到新值
        std::fs::write(&path, r#"{"listen":"127.0.0.1:2222"}"#).unwrap();
        assert_eq!(load().listen.as_deref(), Some("127.0.0.1:2222"));

        // 删除后回落默认
        std::fs::remove_file(&path).unwrap();
        assert_eq!(load(), ProxyConfig::default());
    }

    #[test]
    fn token_prefers_env_then_config() {
        let _env = env_key_lock();
        let _g = AgentDirGuard::temp();
        let cfg = ProxyConfig {
            api_key: Some("from-config".into()),
            ..Default::default()
        };
        // 环境变量未设置：用配置文件
        unsafe { std::env::remove_var(TOKEN_ENV) };
        assert_eq!(token(&cfg).as_deref(), Some("from-config"));
        assert_eq!(
            token_with_source(&cfg),
            Some(("from-config".into(), TokenSource::Config))
        );

        unsafe { std::env::set_var(TOKEN_ENV, "from-env") };
        assert_eq!(token(&cfg).as_deref(), Some("from-env"));
        assert_eq!(
            token_with_source(&cfg),
            Some(("from-env".into(), TokenSource::Env))
        );
        unsafe { std::env::remove_var(TOKEN_ENV) };

        // 空值视同未设置
        let empty = ProxyConfig {
            api_key: Some("  ".into()),
            ..Default::default()
        };
        assert_eq!(token(&empty), None);
        assert_eq!(token_with_source(&empty), None);
    }
}
