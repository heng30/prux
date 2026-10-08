//! 远程模型目录刷新
//!
//! models-store.json 只保存「当前支持 provider」的远程目录缓存：
//! - 静态基线（代码内嵌）始终可用，远程目录按 id 覆盖/追加（见 model_resolver::provider_models）
//! - 启动时对缺失条目的 provider 异步拉取；/login 成功后强制刷新该 provider
//! - 端点：`{CATALOG_BASE_URL}/api/models/providers/{provider_id}`（无认证公开接口）
//! - 缓存：4 小时窗口（checkedAt）；ETag 条件请求（304 只更新 checkedAt）；
//!   404/501 置空单；其他失败保留缓存并报错（下次再验证）

use crate::{
    core::{
        auth::has_auth,
        config_cache::ConfigCache,
        settings_manager::{agent_dir, atomic_write_config},
    },
    error::{Error, Result},
    utils::{
        http::{self, urlencode},
        time::now_ms,
    },
};
use serde_json::{Value, json};
use std::{fs, path::PathBuf, sync::RwLock};

/// 远程目录服务
pub const CATALOG_BASE_URL: &str = "https://pi.dev";
/// 远程目录刷新间隔
pub const REFRESH_INTERVAL_MS: u64 = 4 * 60 * 60 * 1000;
/// 单次刷新超时
const REFRESH_TIMEOUT_MS: u64 = 15_000;
/// 远程目录请求的模型类型：
/// 不传该参数时服务端只回 chat-only 分片，image / classifier 条目在远程刷新里永远拿不到。
const REMOTE_CATALOG_MODEL_TYPES: &str = "chat,image,classifier";

/// 刷新结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshStatus {
    /// 成功拉取并更新了目录
    Updated,
    /// 304：内容未变化，仅推进缓存窗
    Unchanged,
    /// 远程无此 provider 目录：清空动态层（保留基线）
    NotAvailable,
    /// 缓存窗内（4h）且已有条目，跳过网络请求
    Skipped,
}

/// 刷新单个 provider 的远程目录
/// force=true 时忽略 4h 缓存窗（/login 成功后调用）。
pub async fn refresh_provider_models(provider: &str, force: bool) -> Result<RefreshStatus> {
    let stored = read_stored_entry(provider);

    // 缓存窗：有条目且 4h 内且非 force → 跳过网络
    if !force && let Some(s) = &stored {
        let fresh = s
            .get("checkedAt")
            .and_then(|v| v.as_u64())
            .map(|t| now_ms().saturating_sub(t) < REFRESH_INTERVAL_MS);
        let has_last_modified = s.get("lastModified").is_some();
        if fresh == Some(true) && has_last_modified {
            return Ok(RefreshStatus::Skipped);
        }
    }

    let url = catalog_url(provider);
    let client = http::client_builder(None).build()?;
    let mut req = client
        .get(&url)
        .timeout(std::time::Duration::from_millis(REFRESH_TIMEOUT_MS))
        .header("accept", "application/json")
        .header("user-agent", concat!("prux/", env!("CARGO_PKG_VERSION")));
    // 有缓存的模型体才携带 ETag 验证器（304 永远不会清空动态层）
    let validator = stored
        .as_ref()
        .and_then(|s| s.get("models"))
        .and_then(|m| m.as_array())
        .filter(|a| !a.is_empty())
        .and_then(|_| stored.as_ref().and_then(|s| s.get("etag")))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    if let Some(etag) = &validator {
        req = req.header("if-none-match", etag);
    }
    let checked_at = now_ms();
    let response = req.send().await.map_err(|source| Error::Io {
        context: format!("Model catalog request failed for {}: {}", provider, source),
        source: std::io::Error::other(source.to_string()),
    })?;
    // headers 读取必须先于 body 消费
    let etag = response
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let status = response.status();

    if status == 304 {
        // 未变化：动态层已由 stored 提供，只推进缓存窗
        let mut e = stored.clone().unwrap_or_else(|| json!({ "models": [] }));
        if let Some(obj) = e.as_object_mut() {
            obj.insert("checkedAt".into(), json!(checked_at));
        }
        write_stored_entry(provider, e)?;
        return Ok(RefreshStatus::Unchanged);
    } else if status == 404 || status == 501 {
        // 远程无此 provider：清空动态层（基线仍可用）
        let mut e = stored.clone().unwrap_or_else(|| json!({ "models": [] }));
        if let Some(obj) = e.as_object_mut() {
            obj.insert("models".into(), json!([]));
            obj.insert("checkedAt".into(), json!(checked_at));
            obj.insert("lastModified".into(), json!(0));
            obj.remove("etag");
        }
        write_stored_entry(provider, e)?;
        return Ok(RefreshStatus::NotAvailable);
    } else if !status.is_success() {
        // 瞬时失败：保留缓存与验证器（etag），下次重验，不覆盖动态层
        let mut e = stored.clone().unwrap_or_else(|| json!({ "models": [] }));
        if let Some(obj) = e.as_object_mut() {
            obj.insert("checkedAt".into(), json!(checked_at));
        }
        _ = write_stored_entry(provider, e);
        return Err(Error::msg(format!(
            "Model catalog request failed for {}: {}",
            provider, status
        )));
    }

    let text = response.text().await.map_err(|source| Error::Io {
        context: format!(
            "Failed to read model catalog body for {}: {}",
            provider, source
        ),
        source: std::io::Error::other(source.to_string()),
    })?;
    let value: Value = serde_json::from_str(&text).map_err(|source| Error::Json {
        context: format!("Invalid model catalog for provider \"{}\"", provider),
        source,
    })?;
    let refreshed = parse_catalog(provider, &value);
    // lastModified 保持 0：prux 无 localGeneratedAt 语义（仅要求字段存在以启用缓存窗）
    let mut entry = json!({
        "models": refreshed,
        "checkedAt": checked_at,
        "lastModified": 0,
    });
    if let Some(e) = etag {
        entry
            .as_object_mut()
            .unwrap()
            .insert("etag".into(), json!(e));
    }
    write_stored_entry(provider, entry)?;
    Ok(RefreshStatus::Updated)
}

/// 单 provider 目录请求 URL：`{base}/api/models/providers/{provider}?types=chat,image,classifier`。
/// provider id 与类型列表都做百分号编码（逗号 → `%2C`）。
fn catalog_url(provider: &str) -> String {
    format!(
        "{}/api/models/providers/{}?types={}",
        CATALOG_BASE_URL,
        urlencode(provider),
        urlencode(REMOTE_CATALOG_MODEL_TYPES)
    )
}

/// 解析远程目录响应：数组 / {models: []} / 对象值任一形态；
/// 过滤带 id 的条目并补 provider 字段
fn parse_catalog(provider: &str, value: &Value) -> Vec<Value> {
    let entries = match value {
        Value::Array(a) => a.clone(),
        Value::Object(o) => {
            if let Some(models) = o.get("models").and_then(|m| m.as_array()) {
                models.clone()
            } else {
                o.values().cloned().collect()
            }
        }
        _ => Vec::new(),
    };
    entries
        .into_iter()
        .filter(|m| m.get("id").is_some())
        .map(|mut m| {
            if let Some(obj) = m.as_object_mut() {
                obj.insert("provider".into(), json!(provider));
            }
            m
        })
        .collect()
}

/// agent 目录下 models-store.json 的完整路径。
pub fn models_store_path() -> PathBuf {
    agent_dir().join("models-store.json")
}

/// models-store.json 读写锁（本文件自己的锁；读多写少）。
static MODELS_STORE_LOCK: RwLock<()> = RwLock::new(());

/// models-store.json 的缓存层。读/写都只在 [`with_store_read`] / [`with_store_write`]
/// 的 `RwLock` 作用域内进行，锁序恒为「RwLock → 缓存 Mutex」。
static MODELS_STORE_CACHE: ConfigCache = ConfigCache::new();

/// 在 models-store.json 读锁内执行闭包。
fn with_store_read<R>(f: impl FnOnce() -> R) -> R {
    let _guard = MODELS_STORE_LOCK.read().unwrap_or_else(|e| e.into_inner());
    f()
}

/// 在 models-store.json 写锁内执行闭包（读-改-写整体在锁内）。
fn with_store_write<R>(f: impl FnOnce() -> R) -> R {
    let _guard = MODELS_STORE_LOCK.write().unwrap_or_else(|e| e.into_inner());
    f()
}

/// 读取 models-store.json 整表（持读锁，走 [`MODELS_STORE_CACHE`]）；文件不存在/非法 → None。
pub(crate) fn read_models_store() -> Option<Value> {
    with_store_read(read_models_store_unlocked)
}

/// 不加锁读取整表（调用方已持 store 锁：读路径与写路径的 read-modify-write 共用）。
/// 走缓存：命中直接返回内存值，未命中读盘并回填。
fn read_models_store_unlocked() -> Option<Value> {
    let path = models_store_path();
    let v = MODELS_STORE_CACHE.read(&path, || {
        fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or(Value::Null)
    });

    if v.is_null() { None } else { Some(v) }
}

/// 读取 models-store.json 中某 provider 的缓存条目。
pub fn read_stored_entry(provider: &str) -> Option<Value> {
    read_models_store().and_then(|v| v.get(provider).cloned())
}

/// 无条件写回 models-store.json（权限：0600；调用方负责持写锁）。
///
/// 原子写（临时文件 + rename）：避免 truncate→write 窗口被并发读者/进程看到空文件。
fn write_store_value(v: &Value) -> Result<()> {
    let path = models_store_path();
    let text = serde_json::to_string_pretty(v).map_err(|source| Error::Json {
        context: "Failed to serialize models-store.json".to_string(),
        source,
    })?;
    atomic_write_config(&path, text.as_bytes(), Some(0o600))?;
    MODELS_STORE_CACHE.store(&path, v.clone()); // 落盘成功才回填缓存（取写盘后的元数据）。
    Ok(())
}

/// 在写锁内对 models-store.json 做一次 read-modify-write；`f` 返回 true 才写回。
fn modify_models_store<F>(f: F) -> Result<()>
where
    F: FnOnce(&mut serde_json::Map<String, Value>) -> bool,
{
    with_store_write(|| {
        // 调用方已持写锁：走缓存读基准（不再取读锁）。
        let mut v: Value = read_models_store_unlocked().unwrap_or_else(|| json!({}));
        if !v.is_object() {
            v = json!({});
        }
        let obj = v
            .as_object_mut()
            .ok_or_else(|| Error::msg("models-store.json is not an object"))?;
        if !f(obj) {
            return Ok(());
        }
        write_store_value(&v)
    })
}

/// 写回 models-store.json 中某 provider 的缓存条目（权限：0600）
pub fn write_stored_entry(provider: &str, entry: Value) -> Result<()> {
    modify_models_store(|obj| {
        obj.insert(provider.to_string(), entry);
        true
    })
}

/// 移除某 provider 的缓存条目（/logout 成功后调用，保持 store 洁净）
pub fn remove_stored_entry(provider: &str) -> Result<()> {
    modify_models_store(|obj| obj.remove(provider).is_some())
}

/// 启动时清理无凭据的 store 条目（对齐用户契约：models-store.json 只保存
/// 已 /login 的 provider；残留条目在启动时清扫，保持文件洁净）
pub fn cleanup_stale_entries() {
    // 无凭据的条目才清理；无变化不写回（不打脏文件 mtime）
    _ = modify_models_store(|obj| {
        let stale: Vec<String> = obj.keys().filter(|p| !has_auth(p)).cloned().collect();
        if stale.is_empty() {
            return false;
        }
        for p in &stale {
            obj.remove(p);
        }
        true
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_catalog_accepts_all_shapes() {
        // 数组形态
        let a = json!([{"id": "m1"}, {"id": "m2", "name": "M2"}]);
        let out = parse_catalog("deepseek", &a);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["provider"], "deepseek");
        // {models: [...]} 形态
        let o = json!({"models": [{"id": "m3"}]});
        let out = parse_catalog("opencode", &o);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["provider"], "opencode");
        // 对象值形态
        let g = json!({"group": {"id": "m4"}});
        let out = parse_catalog("opencode-go", &g);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["id"], "m4");
        assert_eq!(out[0]["provider"], "opencode-go");
        // 无 id 的条目被过滤
        let bad = json!([{"id": "m5"}, {"notId": true}]);
        assert_eq!(parse_catalog("deepseek", &bad).len(), 1);
        // 非法响应 → 空
        assert!(parse_catalog("deepseek", &json!("nope")).is_empty());
    }

    #[test]
    fn urlencode_keeps_simple_ids() {
        assert_eq!(urlencode("opencode-go"), "opencode-go");
        assert_eq!(urlencode("a/b c"), "a%2Fb%20c");
    }

    /// 请求必须带上 `types=chat,image,classifier`，否则服务端只回 chat-only 分片（pi 0.99.0）。
    #[test]
    fn catalog_url_requests_every_model_type() {
        assert_eq!(
            catalog_url("opencode-go"),
            "https://pi.dev/api/models/providers/opencode-go?types=chat%2Cimage%2Cclassifier"
        );
        assert_eq!(
            catalog_url("a/b c"),
            "https://pi.dev/api/models/providers/a%2Fb%20c?types=chat%2Cimage%2Cclassifier"
        );
    }

    #[test]
    fn stored_entry_roundtrip() {
        let _ad = crate::test_support::AgentDirGuard::temp();
        let _ = fs::remove_file(models_store_path());
        assert!(read_stored_entry("deepseek").is_none());
        write_stored_entry(
            "deepseek",
            json!({"models": [{"id": "m1"}], "checkedAt": 1}),
        )
        .unwrap();
        let e = read_stored_entry("deepseek").unwrap();
        assert_eq!(e["models"][0]["id"], "m1");
        // 追加第二个 provider 不覆盖
        write_stored_entry("opencode", json!({"models": []})).unwrap();
        assert!(read_stored_entry("deepseek").is_some());
        assert!(read_stored_entry("opencode").is_some());
        let _ = fs::remove_file(models_store_path());
    }

    fn lock_and_dir() -> crate::test_support::AgentDirGuard {
        // 每测试独立临时目录：残留问题由构造消灭（无需手动清场）
        crate::test_support::AgentDirGuard::temp()
    }

    #[test]
    fn store_cache_reflects_external_edits() {
        // 缓存以 (path, mtime, len) 为有效键：外部（非本模块 API）改写/删除
        // models-store.json 后，下一次读取必须看到新值。
        let _ad = crate::test_support::AgentDirGuard::temp();
        let path = models_store_path();
        let _ = fs::remove_file(&path);

        std::fs::write(&path, r#"{"deepseek":{"models":[{"id":"a"}]}}"#).unwrap();
        assert_eq!(
            read_stored_entry("deepseek").unwrap()["models"][0]["id"],
            "a"
        );

        // 外部改写（长度也不同）→ 缓存失效，读到新条目
        std::fs::write(&path, r#"{"deepseek":{"models":[{"id":"bb"}]}}"#).unwrap();
        assert_eq!(
            read_stored_entry("deepseek").unwrap()["models"][0]["id"],
            "bb"
        );

        // 删除文件 → 无条目
        let _ = fs::remove_file(&path);
        assert!(read_stored_entry("deepseek").is_none());
    }

    #[test]
    fn concurrent_store_writes_do_not_lose_updates() {
        // models-store.json 的读-改-写必须串行（并发刷新不同 provider 不得丢缓存）。
        // spawn 线程读不到线程本地 override → 用进程级 PRUX_AGENT_DIR + AUTH_TEST_LOCK 串行。
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::test_support::pin_test_agent_dir();
        let _ = fs::remove_file(models_store_path());

        let n: u64 = 16;
        std::thread::scope(|s| {
            for i in 0..n {
                s.spawn(move || {
                    for _ in 0..20 {
                        write_stored_entry(
                            &format!("prov{i}"),
                            json!({"models": [{"id": format!("m{i}")}]}),
                        )
                        .unwrap();
                    }
                });
            }
        });

        for i in 0..n {
            assert!(
                read_stored_entry(&format!("prov{i}")).is_some(),
                "并发写入丢失 provider prov{i}"
            );
        }
        let _ = fs::remove_file(models_store_path());
    }

    #[test]
    fn cleanup_removes_providers_without_credentials() {
        let _g = lock_and_dir();
        let _ = fs::remove_file(models_store_path());
        // store 有 A/B 两条目录，auth 仅 A
        write_stored_entry("deepseek", json!({"models": [{"id": "a1"}]})).unwrap();
        write_stored_entry("opencode", json!({"models": [{"id": "b1"}]})).unwrap();
        crate::core::auth::write_auth_key("deepseek", "sk-a").unwrap();

        cleanup_stale_entries();

        assert!(
            read_stored_entry("deepseek").is_some(),
            "有凭据的 provider 保留"
        );
        assert!(
            read_stored_entry("opencode").is_none(),
            "无凭据的 provider 清理"
        );
        // 文件内容只剩 deepseek
        let v: Value =
            serde_json::from_str(&fs::read_to_string(models_store_path()).unwrap()).unwrap();
        assert_eq!(v.as_object().unwrap().len(), 1);
        // 清空 auth → 全部清理
        crate::core::auth::remove_auth("deepseek").unwrap();
        cleanup_stale_entries();
        let v: Value =
            serde_json::from_str(&fs::read_to_string(models_store_path()).unwrap()).unwrap();
        assert!(v.as_object().unwrap().is_empty(), "无凭据后 store 应为空");
        let _ = fs::remove_file(models_store_path());
    }

    #[test]
    fn cleanup_noop_when_store_missing() {
        let _g = lock_and_dir();
        let _ = fs::remove_file(models_store_path());
        // 不应 panic、不应创建文件
        cleanup_stale_entries();
        assert!(!models_store_path().exists());
    }

    #[test]
    fn remove_entry_cleans_single_provider() {
        let _g = lock_and_dir();
        let _ = fs::remove_file(models_store_path());
        write_stored_entry("deepseek", json!({"models": []})).unwrap();
        write_stored_entry("opencode", json!({"models": []})).unwrap();
        remove_stored_entry("deepseek").unwrap();
        assert!(read_stored_entry("deepseek").is_none());
        assert!(read_stored_entry("opencode").is_some());
        // 删除不存在的条目不报错
        remove_stored_entry("nope").unwrap();
        let _ = fs::remove_file(models_store_path());
    }
}
