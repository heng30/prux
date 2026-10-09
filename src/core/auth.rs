//! 凭据存储（auth.json）与 API key / OAuth 凭据解析
//!
//! auth.json 条目形如 `{provider: {type: "api_key"|"oauth", ...}}`。本模块负责：
//! - 凭据读写（/login、/logout、OAuth 刷新写回）；
//! - API key 解析链路（`--api-key` > auth.json > OAuth access > models.json > 环境变量）；
//! - provider 可用性判定（auth.json ∪ 环境变量 ∪ models.json 配置了 apiKey）；
//! - OAuth 凭据过期时的刷新与写回。
//!
//! 读写走本文件自己的 [`AUTH_LOCK`] + [`AUTH_CACHE`]（与其它配置文件同构）：
//! - 读：`(path, mtime, len)` 命中直接返回内存值，否则读盘并回填；
//! - 写：在写锁内完成整个读-改-写（落盘成功后回填缓存），因此并发 /login /logout 不丢条目。

use super::virtual_models;
use crate::{
    APP_NAME,
    core::{
        config_cache::ConfigCache,
        login_registry, model_config,
        oauth::{self, OAuthCredential},
        settings_manager::{agent_dir, atomic_write_config},
    },
    error::{Error, Result},
    utils::{mime::strip_bom, time::now_ms},
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Mutex, OnceLock, RwLock},
};

/// OAuth 刷新超时
pub const OAUTH_REFRESH_TIMEOUT_MS: u64 = 15_000;

/// Anthropic workload identity federation 的**必填**环境变量：三者齐备且非空才算「已配置」。
pub const ANTHROPIC_FEDERATION_REQUIRED: [&str; 3] = [
    "ANTHROPIC_FEDERATION_RULE_ID",
    "ANTHROPIC_ORGANIZATION_ID",
    "ANTHROPIC_IDENTITY_TOKEN_FILE",
];

/// Anthropic workload identity federation 的**可选**环境变量：部分部署需要，只用于提示。
pub const ANTHROPIC_FEDERATION_OPTIONAL: [&str; 2] =
    ["ANTHROPIC_SERVICE_ACCOUNT_ID", "ANTHROPIC_WORKSPACE_ID"];

/// 每个 provider 一把刷新锁（避免并发刷新同一 provider 浪费/竞争 refresh token）。
static OAUTH_REFRESH_LOCKS: OnceLock<Mutex<HashMap<String, ()>>> = OnceLock::new();

/// auth.json 读写锁（/login、OAuth 刷新、/logout 可能并发写同一文件）。
static AUTH_LOCK: RwLock<()> = RwLock::new(());

/// auth.json 的缓存层。读/写都只在 [`with_auth_read`] / [`with_auth_write`] 的
/// `RwLock` 作用域内进行，锁序恒为「RwLock → 缓存 Mutex」。
static AUTH_CACHE: ConfigCache = ConfigCache::new();

/// 在 auth.json 读锁内执行闭包。
fn with_auth_read<R>(f: impl FnOnce() -> R) -> R {
    let _guard = AUTH_LOCK.read().unwrap_or_else(|e| e.into_inner());
    f()
}

/// 在 auth.json 写锁内执行闭包（读-改-写整体在锁内）。
fn with_auth_write<R>(f: impl FnOnce() -> R) -> R {
    let _guard = AUTH_LOCK.write().unwrap_or_else(|e| e.into_inner());
    f()
}

/// 直读 auth.json 中该 provider 的 api_key 字段（不做插值/命令展开）；
/// 缺失或字段非法时报错（CLI `auth bearer-token` 等需要强报错路径）。
pub fn read_auth_key(provider: &str) -> Result<String> {
    with_auth_read(|| {
        let path = agent_dir().join("auth.json");
        let text = std::fs::read_to_string(&path).map_err(|source| Error::Io {
            context: format!("Failed to read auth.json: {}", path.display()),
            source,
        })?;
        let v: Value = serde_json::from_str(strip_bom(&text)).map_err(|source| Error::Json {
            context: "Failed to parse auth.json".to_string(),
            source,
        })?;
        let cred = v
            .get(provider)
            .ok_or_else(|| Error::msg(format!("provider not found in auth.json: {}", provider)))?;
        let key = cred.get("key").and_then(|k| k.as_str()).ok_or_else(|| {
            Error::msg(format!(
                "credentials for provider {} are missing key",
                provider
            ))
        })?;
        Ok(key.to_string())
    })
}

/// auth.json api_key 值解析：
/// - key 写 `!command` → 执行命令取首行（进程级缓存）
/// - `$ENV`/`${ENV}` → 环境变量插值；`$$`/`$!` 转义
/// - env 对象 → 注入 provider 作用域环境（当前实现并入进程判定，调用方经 get_auth_env 读取）
pub fn resolve_auth_key(provider: &str) -> Option<String> {
    let v = read_auth_value();
    let cred = v.get(provider)?;
    let raw = cred.get("key").and_then(|k| k.as_str())?;
    let resolved = resolve_key_with_interpolation(raw, cred);
    Some(resolved)
}

/// auth.json 该 provider 声明的 env 对象（provider 作用域环境注入；优先级高于进程 env）
pub fn get_auth_env(provider: &str) -> Option<Value> {
    read_auth_value()
        .get(provider)
        .and_then(|c| c.get("env").cloned())
}

/// key 值插值：`!cmd` 执行、`$ENV`/`${ENV}` 展开、`$$`/`$!` 转义
fn resolve_key_with_interpolation(raw: &str, _cred: &Value) -> String {
    /// `!command` 的进程级缓存（原始 key 串 → 命令首行），避免每次解析都 fork。
    static CMD_CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

    if let Some(cmd) = raw.strip_prefix('!') {
        let cache = CMD_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
        if let Ok(g) = cache.lock()
            && let Some(v) = g.get(raw)
        {
            return v.clone();
        }

        if let Ok(out) = std::process::Command::new("sh").arg("-c").arg(cmd).output()
            && out.status.success()
        {
            let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if let Ok(mut g) = cache.lock() {
                g.insert(raw.to_string(), value.clone());
            }
            return value;
        }
        return String::new();
    }

    // $ENV / ${ENV} 插值 + $$/$! 转义
    let mut out = String::new();
    let bytes = raw.as_bytes();
    let mut i = 0usize;
    let mut prev_dollar = false;

    while i < bytes.len() {
        let c = bytes[i];
        if prev_dollar {
            match c {
                b'$' => {
                    out.push('$');
                }
                b'!' => {
                    out.push('!');
                }
                b'{' => {
                    // ${NAME}
                    if let Some(end) = raw[i + 1..].find('}') {
                        let name = &raw[i + 1..i + 1 + end];
                        out.push_str(&resolve_env_name(name));
                        i += 1 + end;
                    } else {
                        out.push_str("${");
                    }
                }
                _ => {
                    // $NAME
                    let mut end = i;
                    while end < bytes.len()
                        && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_')
                    {
                        end += 1;
                    }
                    if end > i {
                        out.push_str(&resolve_env_name(&raw[i..end]));
                        i = end - 1;
                    } else {
                        out.push('$');
                    }
                }
            }
            prev_dollar = false;
        } else if c == b'$' {
            prev_dollar = true;
        } else {
            out.push(c as char);
        }
        i += 1;
    }

    if prev_dollar {
        out.push('$');
    }

    out
}

/// `$NAME` / `${NAME}` 展开：取进程环境变量，未设置展开为空串。
fn resolve_env_name(name: &str) -> String {
    if name.is_empty() {
        return String::new();
    }
    std::env::var(name).unwrap_or_default()
}

/// auth.json 绝对路径
pub fn auth_path() -> PathBuf {
    agent_dir().join("auth.json")
}

/// 读取 auth.json 的原始内容（不存在/非法返回 Null）。
fn read_auth_value() -> Value {
    with_auth_read(read_auth_value_unlocked)
}

/// 不加锁读取 auth.json（调用方已持 auth 锁：写路径 read-modify-write 用）。
/// 走 [`AUTH_CACHE`]：命中直接返回内存值，未命中读盘并回填。
fn read_auth_value_unlocked() -> Value {
    let path = auth_path();
    AUTH_CACHE.read(&path, || {
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or(Value::Null)
    })
}

/// 写回 auth.json（权限：0600）
fn write_auth_value(v: &Value) -> Result<()> {
    let path = auth_path();
    let text = serde_json::to_string_pretty(v).map_err(|source| Error::Json {
        context: "Failed to serialize auth.json".to_string(),
        source,
    })?;

    // 原子写 + 临时文件阶段就收紧到 0600：敏感凭据不得以默认权限短暂暴露。
    atomic_write_config(&path, text.as_bytes(), Some(0o600))?;
    // 落盘成功才回填缓存（取写盘后的元数据），避免缓存持有未写盘的值。
    AUTH_CACHE.store(&path, v.clone());
    Ok(())
}

/// 在写锁内对 auth.json 做一次 read-modify-write（保留其他 provider 条目）。
/// `f` 接收当前对象并就地修改，返回值透传（调用方据此判断是否命中）。
fn modify_auth_value<F, R>(f: F) -> Result<R>
where
    F: FnOnce(&mut serde_json::Map<String, Value>) -> R,
{
    with_auth_write(|| {
        let mut v = read_auth_value_unlocked();
        if !v.is_object() {
            v = json!({});
        }

        let obj = v
            .as_object_mut()
            .ok_or_else(|| Error::msg("auth.json is not an object"))?;
        let out = f(obj);
        write_auth_value(&v)?;
        Ok(out)
    })
}

/// 保存 API key 凭据 {type: "api_key", key: "your-api-key"}）
pub fn write_auth_key(provider: &str, key: &str) -> Result<()> {
    modify_auth_value(|obj| {
        obj.insert(
            provider.to_string(),
            serde_json::json!({
                "type": "api_key",
                "key": key,
            }),
        );
    })
}

/// 删除 provider 凭据
pub fn remove_auth(provider: &str) -> Result<()> {
    let removed = modify_auth_value(|obj| obj.remove(provider).is_some())?;
    if !removed {
        return Err(Error::msg(format!(
            "no stored credentials for provider: {}",
            provider
        )));
    }
    Ok(())
}

/// 列出 auth.json 中有凭据条目的 provider
pub fn list_auth_providers() -> Vec<String> {
    let v = read_auth_value();
    match v.as_object() {
        Some(obj) => obj.keys().cloned().collect(),
        None => Vec::new(),
    }
}

/// provider 是否已存凭据（auth.json 中有条目）
pub fn has_auth(provider: &str) -> bool {
    list_auth_providers().iter().any(|p| p == provider)
}

/// 读取 auth.json 中 provider 的凭据条目
/// 返回 (凭据类型, 令牌)：api_key → key；oauth → access。
pub fn read_auth_entry(provider: &str) -> Option<(String, String)> {
    let v = read_auth_value();
    let entry = v.get(provider)?;
    let cred_type = entry.get("type").and_then(|x| x.as_str())?.to_string();
    let token = entry
        .get("key")
        .or_else(|| entry.get("access"))
        .and_then(|x| x.as_str())?
        .to_string();
    Some((cred_type, token))
}

/// 列出 auth.json 中有凭据条目的 (provider, 凭据类型)
pub fn list_auth_types() -> Vec<(String, String)> {
    list_auth_providers()
        .into_iter()
        .filter_map(|p| read_auth_entry(&p).map(|(t, _)| (p, t)))
        .collect()
}

/// provider 对应的 API key 环境变量名
pub fn api_key_env_var(provider: &str) -> Option<&'static str> {
    Some(match provider {
        "deepseek" => "DEEPSEEK_API_KEY",
        "opencode" | "opencode-go" => "OPENCODE_API_KEY",
        "anthropic" => "ANTHROPIC_API_KEY",
        "ant-ling" => "ANT_LING_API_KEY",
        "baseten" => "BASETEN_API_KEY",
        "cerebras" => "CEREBRAS_API_KEY",
        "fireworks" => "FIREWORKS_API_KEY",
        "github-copilot" => "COPILOT_GITHUB_TOKEN",
        "google" => "GEMINI_API_KEY",
        "groq" => "GROQ_API_KEY",
        "huggingface" => "HF_TOKEN",
        "kimi-coding" => "KIMI_API_KEY",
        "meta" => "META_API_KEY",
        "minimax" => "MINIMAX_API_KEY",
        "minimax-cn" => "MINIMAX_CN_API_KEY",
        "moonshotai" | "moonshotai-cn" => "MOONSHOT_API_KEY",
        "nvidia" => "NVIDIA_API_KEY",
        "openai" => "OPENAI_API_KEY",
        "openrouter" => "OPENROUTER_API_KEY",
        "qwen-token-plan" | "qwen-token-plan-individual" => "QWEN_TOKEN_PLAN_API_KEY",
        "qwen-token-plan-cn" => "QWEN_TOKEN_PLAN_CN_API_KEY",
        "together" => "TOGETHER_API_KEY",
        "typesafe" => "TYPESAFE_API_KEY",
        "vercel-ai-gateway" => "AI_GATEWAY_API_KEY",
        "xai" => "XAI_API_KEY",
        "xiaomi" => "XIAOMI_API_KEY",
        "xiaomi-token-plan-ams" => "XIAOMI_TOKEN_PLAN_AMS_API_KEY",
        "xiaomi-token-plan-cn" => "XIAOMI_TOKEN_PLAN_CN_API_KEY",
        "xiaomi-token-plan-sgp" => "XIAOMI_TOKEN_PLAN_SGP_API_KEY",
        "zai" => "ZAI_API_KEY",
        "zai-coding-cn" => "ZAI_CODING_CN_API_KEY",
        _ => return None,
    })
}

/// 已配置的 Anthropic workload identity federation（必填项齐备）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FederationConfig {
    /// 联合规则 id（`ANTHROPIC_FEDERATION_RULE_ID`）
    pub rule_id: String,
    /// 组织 id（`ANTHROPIC_ORGANIZATION_ID`）
    pub organization_id: String,
    /// 身份令牌文件路径（`ANTHROPIC_IDENTITY_TOKEN_FILE`，K8s 会轮转该文件）
    pub identity_token_file: String,
}

impl FederationConfig {
    /// 身份令牌文件当前是否可读（不可读只影响提示文案，不改变「已配置」判定）。
    pub fn identity_token_readable(&self) -> bool {
        std::fs::read_to_string(&self.identity_token_file)
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)
    }
}

/// 读取 Anthropic federation 配置：必填三项任一缺失或为空时返回 `None`（视为未配置）。
///
/// prux **尚未实现** federation 的 token 交换（见 `migrations/migration-v1.0.2.md` §12.1），
/// 这里只用于识别与报错，避免用户配齐三元组却看到「No API key」这类误导文案。
pub fn anthropic_federation() -> Option<FederationConfig> {
    let value = |name: &str| {
        std::env::var(name)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };

    Some(FederationConfig {
        rule_id: value(ANTHROPIC_FEDERATION_REQUIRED[0])?,
        organization_id: value(ANTHROPIC_FEDERATION_REQUIRED[1])?,
        identity_token_file: value(ANTHROPIC_FEDERATION_REQUIRED[2])?,
    })
}

/// 供用户看的 federation 状态说明（装在错误与启动提示后面）。
///
/// `provider` 不是 `"anthropic"` 时返回 `None`（federation 只有 Anthropic 有）。
/// 仅在「已配 federation、但当前版本不支持」时返回 `Some`：
/// 已配请求级凭据（`ANTHROPIC_API_KEY` / `ANTHROPIC_AUTH_TOKEN` / `/login`）时凭据优先，不再提示。
pub fn federation_note(provider: &str) -> Option<String> {
    if provider != "anthropic" {
        return None;
    }
    let config = anthropic_federation()?;
    if resolve_env_api_key("anthropic").is_some()
        || resolve_auth_key("anthropic").is_some()
        || read_oauth_credential("anthropic").is_some()
    {
        return None;
    }

    let file_state = if config.identity_token_readable() {
        "identity token file is readable"
    } else {
        "identity token file is missing or empty"
    };
    Some(format!(
        "Anthropic workload identity federation is configured ({}), but this {APP_NAME} version has no \
         federation token exchange yet ({file_state}). Use ANTHROPIC_API_KEY / ANTHROPIC_AUTH_TOKEN \
         or run /login instead.",
        ANTHROPIC_FEDERATION_REQUIRED.join(" / ")
    ))
}

/// 按 provider 读取 API key 环境变量（anthropic 额外支持 AUTH_TOKEN / OAUTH_TOKEN 回退）。
fn resolve_env_api_key(provider: &str) -> Option<String> {
    // provider 环境变量来源：anthropic 还支持 ANTHROPIC_AUTH_TOKEN / ANTHROPIC_OAUTH_TOKEN 两个回退来源。
    if provider == "anthropic" {
        for name in [
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_OAUTH_TOKEN",
        ] {
            if let Ok(v) = std::env::var(name)
                && !v.trim().is_empty()
            {
                return Some(v);
            }
        }
        return None;
    }
    api_key_env_var(provider).and_then(|name| std::env::var(name).ok())
}

/// 解析 API key：--api-key flag > auth.json API key > auth.json OAuth access > models.json apiKey（值解析）> 环境变量。
/// OAuth 凭据（anthropic/copilot/kimi/xai/openai/openrouter 等）以 access token 充当 api_key；
/// 过期时由 ensure_oauth_valid 在网络层刷新，这里只负责同步填充初始值。
/// models.json 的 provider 级 apiKey 是用户的显式配置：**声明了就只认它**，
/// 值引用不可解析时视为不可用、**不回退**内置环境变量探测
pub fn resolve_api_key(provider: &str, auth_value: Option<String>) -> Option<String> {
    if let Some(k) = auth_value
        && !k.trim().is_empty()
    {
        return Some(k);
    }

    if let Some(k) = resolve_auth_key(provider)
        && !k.trim().is_empty()
    {
        return Some(k);
    }

    if let Some(cred) = read_oauth_credential(provider)
        && !cred.access.trim().is_empty()
    {
        return Some(cred.access);
    }

    if let Some(raw) = model_config::provider_config(provider).and_then(|c| c.api_key) {
        return model_config::resolve_config_value(&raw).filter(|k| !k.trim().is_empty());
    }

    if let Some(k) = resolve_env_api_key(provider).filter(|k| !k.trim().is_empty()) {
        return Some(k);
    }

    None
}

/// 已配置凭据的 provider 来源判定（对齐 pi `composeApiKeyAuth.check`）：
/// auth.json 凭据优先；其次是 models.json 的 provider 级 apiKey——**声明了就以它为准**
/// （值引用不可解析即「未配置」，不再把内置环境变量当作来源）；都没声明才回退环境变量。
/// /model 面板、Ctrl+P 循环、启动检查共用
pub fn list_configured_providers() -> Vec<String> {
    let mut out = list_auth_providers();
    let configs = model_config::read_models_config();

    for (id, cfg) in &configs {
        if cfg.has_configured_key() && !out.contains(id) {
            out.push(id.clone());
        }
    }

    // 内置环境变量命中的 provider（environment 来源视为已配置）：models.json 未声明 apiKey 时才算
    for p in login_registry::LOGIN_PROVIDERS {
        if configs.get(p.id).is_some_and(|c| c.api_key.is_some()) {
            continue;
        }

        if env_key_source(p.id).is_some() && !out.iter().any(|x| x.as_str() == p.id) {
            out.push(p.id.to_string());
        }
    }

    // 仅有虚拟模型的 provider（无物理条目、无凭据）始终可用。
    // 物理 provider 上的虚拟模型仍随该 provider 的凭据可用性过滤。
    for p in virtual_models::virtual_only_providers() {
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// provider 是否由环境变量提供 API key；返回命中且非空的环境变量名。
/// /login 面板用（desc 显示 `✓ env (VAR)`），可用性判定不执行命令。
pub fn env_key_source(provider: &str) -> Option<&'static str> {
    api_key_env_var(provider).filter(|name| {
        std::env::var(name)
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false)
    })
}

/// 保存 OAuth 凭据（auth.json 条目 {type: oauth, access, refresh, expires[, 附加字段]}）
pub fn write_oauth_credential(provider: &str, cred: &OAuthCredential) -> Result<()> {
    modify_auth_value(|obj| {
        let mut entry = serde_json::json!({
            "type": "oauth",
            "access": cred.access,
            "refresh": cred.refresh,
            "expires": cred.expires,
        });
        if let Some(ep) = &cred.enterprise_url {
            entry["enterpriseUrl"] = serde_json::json!(ep);
        }
        if let Some(ids) = &cred.available_model_ids {
            entry["availableModelIds"] = serde_json::json!(ids);
        }
        if let Some(id) = &cred.client_id {
            entry["clientId"] = serde_json::json!(id);
        }
        if let Some(scopes) = &cred.scopes {
            entry["scopes"] = serde_json::json!(scopes);
        }
        obj.insert(provider.to_string(), entry);
    })
}

/// 读取 OAuth 凭据（非 oauth 或不存在返回 None）
pub fn read_oauth_credential(provider: &str) -> Option<OAuthCredential> {
    let v = read_auth_value();
    let entry = v.get(provider)?;
    if entry.get("type").and_then(|t| t.as_str()) != Some("oauth") {
        return None;
    }
    Some(OAuthCredential {
        access: entry.get("access")?.as_str()?.to_string(),
        refresh: entry.get("refresh")?.as_str()?.to_string(),
        expires: entry.get("expires")?.as_u64()?,
        enterprise_url: entry
            .get("enterpriseUrl")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string()),
        available_model_ids: entry
            .get("availableModelIds")
            .and_then(|x| x.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            }),
        client_id: entry
            .get("clientId")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string()),
        scopes: entry.get("scopes").and_then(|x| x.as_array()).map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        }),
    })
}

/// 确保 OAuth 凭据有效：过期则按 provider 刷新并写回。
/// 返回可用于请求的 access token；非 oauth 或无需处理返回 None。
/// 刷新失败返回错误（提示重新 /login）。OAuth 刷新并发防护，并发请求只触发一次刷新，写回后才释放锁）
pub async fn ensure_oauth_valid(provider: &str) -> Result<Option<String>> {
    let Some(cred) = read_oauth_credential(provider) else {
        return Ok(None);
    };

    if !cred.expired(now_ms()) {
        return Ok(Some(cred.access));
    }

    if cred.is_permanent() || !cred.refreshable() {
        return Ok(Some(cred.access));
    }

    // double-checked locking：锁内复查过期，避免并发重复刷新
    let locks = OAUTH_REFRESH_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let holder = locks
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(provider)
        .cloned();

    // 其他线程正在刷新本 provider：直接返回旧 token， 由调用方在请求层重试
    if let Some(()) = holder {
        return Ok(Some(cred.access));
    }

    locks
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(provider.to_string(), ());

    // 刷新在**独立任务**里跑：发起方被取消（请求超时、回合中止、会话关闭）不得中断它。
    // provider 一旦答完就可能已轮转 refresh token，丢了就再也拿不回来。
    // 任务结束（无论成败）自己摘掉登记，避免失败/取消把该 provider 永久锁死。
    let provider_owned = provider.to_string();
    let cred_owned = cred.clone();
    let handle = tokio::spawn(async move {
        let outcome = async {
            let refreshed = oauth::refresh_oauth(&provider_owned, &cred_owned)
                .await
                .map_err(|e| e.to_string())?;
            write_oauth_credential(&provider_owned, &refreshed).map_err(|e| e.to_string())?;
            Ok::<OAuthCredential, String>(refreshed)
        }
        .await;
        clear_oauth_refresh_lock(&provider_owned);
        outcome
    });

    let refreshed = match handle.await {
        Ok(Ok(refreshed)) => refreshed,
        Ok(Err(e)) => {
            return Err(Error::msg(format!(
                "OAuth token refresh failed for {}: {}. Please run /login again.",
                provider, e
            )));
        }
        Err(e) => {
            return Err(Error::msg(format!(
                "OAuth token refresh for {} did not complete: {}. Please run /login again.",
                provider, e
            )));
        }
    };

    Ok(Some(refreshed.access))
}

/// 摘掉 provider 的刷新登记（刷新任务结束时调用；无论成败）。
fn clear_oauth_refresh_lock(provider: &str) {
    if let Some(locks) = OAUTH_REFRESH_LOCKS.get() {
        locks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(provider);
    }
}

/// provider 当前可用于请求的 bearer token：优先 OAuth 凭据（过期先刷新），否则 `auth.json` 的 key。
///
/// 两者都没有时返回 `Ok(None)`，由调用方决定是报错还是退回匿名请求。
/// 供 MCP 的 `auth: {"provider": "<name>"}` 复用 provider 的登录结果。
pub async fn resolve_provider_token(provider: &str) -> Result<Option<String>> {
    if let Some(token) = ensure_oauth_valid(provider).await? {
        return Ok(Some(token));
    }
    Ok(resolve_auth_key(provider))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每测试独立临时 agent_dir（线程本地 override）：f 内所有 agent_dir() 调用隔离，
    /// 并行测试互不干扰。
    fn with_temp_agent_dir(f: impl FnOnce()) {
        let _ad = crate::test_support::AgentDirGuard::temp();
        f();
    }

    #[test]
    fn auth_key_roundtrip_and_remove() {
        with_temp_agent_dir(|| {
            let _ = std::fs::remove_file(auth_path());
            assert!(!has_auth("deepseek"));
            assert!(list_auth_providers().is_empty());

            write_auth_key("deepseek", "sk-test-1").unwrap();
            assert!(has_auth("deepseek"));
            assert_eq!(list_auth_providers(), vec!["deepseek".to_string()]);
            assert_eq!(read_auth_key("deepseek").unwrap(), "sk-test-1");

            // 追加第二个 provider，不覆盖已有
            write_auth_key("opencode", "sk-test-2").unwrap();
            assert_eq!(read_auth_key("opencode").unwrap(), "sk-test-2");
            assert_eq!(read_auth_key("deepseek").unwrap(), "sk-test-1");

            // auth.json 权限 0600
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(auth_path()).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o600);
            }

            // 覆盖同 provider（/login 重复配置）
            write_auth_key("deepseek", "sk-test-3").unwrap();
            assert_eq!(read_auth_key("deepseek").unwrap(), "sk-test-3");

            // remove
            remove_auth("deepseek").unwrap();
            assert!(!has_auth("deepseek"));
            assert!(read_auth_key("deepseek").is_err());
            assert_eq!(list_auth_providers(), vec!["opencode".to_string()]);

            // 删除不存在的报错
            assert!(remove_auth("deepseek").is_err());
        });
    }

    /// 2.2：federation 必填三项齐备才算「已配置」；配了凭据时凭据优先，不再提示。
    #[test]
    fn anthropic_federation_is_recognized_and_deferred_to_credentials() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ek = crate::test_support::env_key_lock();
        with_temp_agent_dir(|| {
            let _ = std::fs::remove_file(auth_path());
            for name in ANTHROPIC_FEDERATION_REQUIRED
                .iter()
                .chain(ANTHROPIC_FEDERATION_OPTIONAL.iter())
            {
                unsafe { std::env::remove_var(name) };
            }

            // 缺项（只给两项）→ 视为未配置
            unsafe {
                std::env::set_var(ANTHROPIC_FEDERATION_REQUIRED[0], "rule-1");
                std::env::set_var(ANTHROPIC_FEDERATION_REQUIRED[1], "org-1");
            }
            assert!(anthropic_federation().is_none());
            assert!(federation_note("anthropic").is_none());

            // 三项齐备 → 已配置；identity token 文件不存在时仍算已配置，但文案要点出文件状态
            let missing = tempfile::tempdir().unwrap();
            let missing_path = missing.path().join("no-such-identity-token");
            unsafe {
                std::env::set_var(
                    ANTHROPIC_FEDERATION_REQUIRED[2],
                    missing_path.to_string_lossy().to_string(),
                );
            }
            let config = anthropic_federation().expect("三项齐备应识别为已配置");
            assert_eq!(config.rule_id, "rule-1");
            assert_eq!(config.organization_id, "org-1");
            assert!(!config.identity_token_readable());

            let note = federation_note("anthropic").expect("无凭据时应给出 federation 说明");
            assert!(
                federation_note("deepseek").is_none(),
                "federation 只属 anthropic，其他 provider 不得带上这段文案"
            );
            assert!(note.contains(ANTHROPIC_FEDERATION_REQUIRED[0]), "{note}");
            assert!(note.contains("no federation token exchange"), "{note}");
            assert!(note.contains("ANTHROPIC_API_KEY"), "{note}");
            assert!(note.contains("missing or empty"), "{note}");

            // 文件可读 → 文案改口
            let token = tempfile::tempdir().unwrap();
            let token_path = token.path().join("identity-token");
            std::fs::write(&token_path, "jwt-looking-token\n").unwrap();
            unsafe {
                std::env::set_var(
                    ANTHROPIC_FEDERATION_REQUIRED[2],
                    token_path.to_string_lossy().to_string(),
                );
            }
            assert!(anthropic_federation().unwrap().identity_token_readable());
            assert!(
                federation_note("anthropic")
                    .unwrap()
                    .contains("file is readable")
            );

            // 请求级凭据优先：配了 API key 就不再提示 federation
            unsafe { std::env::set_var("ANTHROPIC_API_KEY", "sk-ant-test") };
            assert!(
                federation_note("anthropic").is_none(),
                "有 API key 时不应提 federation"
            );
            unsafe { std::env::remove_var("ANTHROPIC_API_KEY") };

            // /login（auth.json）存了 key 同样优先
            write_auth_key("anthropic", "sk-ant-stored").unwrap();
            assert!(federation_note("anthropic").is_none());
            remove_auth("anthropic").ok();

            for name in ANTHROPIC_FEDERATION_REQUIRED
                .iter()
                .chain(ANTHROPIC_FEDERATION_OPTIONAL.iter())
            {
                unsafe { std::env::remove_var(name) };
            }
        });
    }

    #[test]
    fn oauth_credential_roundtrip_and_expiry_refresh_path() {
        with_temp_agent_dir(|| {
            let _ = std::fs::remove_file(auth_path());
            let cred = crate::core::oauth::OAuthCredential {
                access: "sk-ant-oat-access".into(),
                refresh: "refresh-1".into(),
                expires: crate::utils::time::now_ms() + 60_000,
                enterprise_url: None,
                available_model_ids: None,
                client_id: None,
                scopes: None,
            };
            write_oauth_credential("anthropic", &cred).unwrap();
            assert!(has_auth("anthropic"));
            // read_auth_entry 兼容 oauth（access 字段）
            let (t, token) = read_auth_entry("anthropic").unwrap();
            assert_eq!(t, "oauth");
            assert_eq!(token, "sk-ant-oat-access");
            let stored = read_oauth_credential("anthropic").unwrap();
            assert_eq!(stored.refresh, "refresh-1");
            // 未过期：ensure_oauth_valid 直接返回 access（不触发网络刷新）
            let rt = tokio::runtime::Runtime::new().unwrap();
            let access = rt.block_on(ensure_oauth_valid("anthropic")).unwrap();
            assert_eq!(access.as_deref(), Some("sk-ant-oat-access"));
            // 非 oauth provider：无操作
            let none = rt.block_on(ensure_oauth_valid("deepseek")).unwrap();
            assert!(none.is_none());
            remove_auth("anthropic").ok();
        });
    }

    /// 刷新失败不得把 provider 永久锁死：登记必须摘掉（pi 1.0.3 / 1.1.0）。
    /// 旧实现里 `?` 早退不会 `remove`，该 provider 之后永远拿到过期 token。
    #[test]
    fn failed_oauth_refresh_releases_the_lock() {
        with_temp_agent_dir(|| {
            let _ = std::fs::remove_file(auth_path());
            let provider = "prux-refresh-test";
            let cred = crate::core::oauth::OAuthCredential {
                access: "stale-access".into(),
                refresh: "refresh-1".into(),
                expires: 1, // 早已过期
                enterprise_url: None,
                available_model_ids: None,
                client_id: None,
                scopes: None,
            };
            write_oauth_credential(provider, &cred).unwrap();

            let in_flight = || {
                OAUTH_REFRESH_LOCKS
                    .get()
                    .map(|l| {
                        l.lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .contains_key(provider)
                    })
                    .unwrap_or(false)
            };

            let rt = tokio::runtime::Runtime::new().unwrap();
            // 未知 provider 的刷新实现 → 立即失败（不发网络请求）
            assert!(rt.block_on(ensure_oauth_valid(provider)).is_err());
            assert!(!in_flight(), "刷新结束后必须摘掉登记");
            // 第二次仍会真的重试，而不是被残留登记挡下、返回过期 token
            assert!(rt.block_on(ensure_oauth_valid(provider)).is_err());
            assert!(!in_flight());

            remove_auth(provider).ok();
        });
    }

    /// MCP 的 `auth: {"provider": ...}` 复用 provider 凭据：OAuth access token 优先，
    /// 否则回落到 `auth.json` 的 key，两者都没有时返回 None。
    #[test]
    fn resolve_provider_token_prefers_oauth_then_api_key() {
        with_temp_agent_dir(|| {
            let _ = std::fs::remove_file(auth_path());
            let rt = tokio::runtime::Runtime::new().unwrap();

            assert!(
                rt.block_on(resolve_provider_token("deepseek"))
                    .unwrap()
                    .is_none(),
                "未登录的 provider 应为 None"
            );

            write_auth_key("deepseek", "sk-key").unwrap();
            assert_eq!(
                rt.block_on(resolve_provider_token("deepseek"))
                    .unwrap()
                    .as_deref(),
                Some("sk-key")
            );

            // 有 OAuth 凭据时优先用 access token
            let cred = OAuthCredential {
                access: "sk-oauth-access".to_string(),
                refresh: "refresh-1".to_string(),
                expires: now_ms() + 3_600_000,
                available_model_ids: None,
                client_id: None,
                scopes: None,
                enterprise_url: None,
            };
            write_oauth_credential("deepseek", &cred).unwrap();
            assert_eq!(
                rt.block_on(resolve_provider_token("deepseek"))
                    .unwrap()
                    .as_deref(),
                Some("sk-oauth-access")
            );

            let _ = std::fs::remove_file(auth_path());
        });
    }

    /// Sign in with ChatGPT：动态签发的 clientId 与 scope 随凭据落盘（刷新要用）
    #[test]
    fn chatgpt_credential_persists_client_id_and_scopes() {
        with_temp_agent_dir(|| {
            let _ = std::fs::remove_file(auth_path());
            let cred = crate::core::oauth::OAuthCredential {
                access: "chatgpt-access".into(),
                refresh: "chatgpt-refresh".into(),
                expires: crate::utils::time::now_ms() + 60_000,
                enterprise_url: None,
                available_model_ids: None,
                client_id: Some("client-issued".into()),
                scopes: Some(vec!["openid".into(), "chatgpt.tokens.use.direct".into()]),
            };
            write_oauth_credential("openai", &cred).unwrap();
            let stored = read_oauth_credential("openai").unwrap();
            assert_eq!(stored.client_id.as_deref(), Some("client-issued"));
            assert_eq!(
                stored.scopes.as_deref(),
                Some(
                    ["openid", "chatgpt.tokens.use.direct"]
                        .map(|s| s.to_string())
                        .as_slice()
                )
            );
            remove_auth("openai").ok();
        });
    }

    #[test]
    fn resolve_api_key_precedence() {
        with_temp_agent_dir(|| {
            let _ek = crate::test_support::env_key_lock();
            let _ = std::fs::remove_file(auth_path());
            // 环境变量兜底
            unsafe {
                std::env::set_var("DEEPSEEK_API_KEY", "sk-env");
            }
            assert_eq!(resolve_api_key("deepseek", None).as_deref(), Some("sk-env"));
            // 凭据优先于环境变量
            write_auth_key("deepseek", "sk-stored").unwrap();
            assert_eq!(
                resolve_api_key("deepseek", None).as_deref(),
                Some("sk-stored")
            );
            // --api-key flag（auth_value）最高优先
            assert_eq!(
                resolve_api_key("deepseek", Some("sk-flag".to_string())).as_deref(),
                Some("sk-flag")
            );
            unsafe {
                std::env::remove_var("DEEPSEEK_API_KEY");
            }
        });
    }

    #[test]
    fn resolve_api_key_uses_oauth_access_as_key() {
        with_temp_agent_dir(|| {
            let _ek = crate::test_support::env_key_lock();
            unsafe { std::env::remove_var("DEEPSEEK_API_KEY") };
            let _ = std::fs::remove_file(auth_path());
            // OAuth 凭据以 access token 充当 api_key。
            // 用专属测试 provider 名，避免与 auth_command 等并行测试共享 github-copilot 条目
            const TEST_PROVIDER: &str = "test-oauth-provider";
            write_oauth_credential(
                TEST_PROVIDER,
                &OAuthCredential {
                    access: "tid=1;proxy-ep=proxy.individual.githubcopilot.com".into(),
                    refresh: "gh-token".into(),
                    expires: now_ms() + 60_000,
                    enterprise_url: Some("github.com".into()),
                    available_model_ids: None,
                    client_id: None,
                    scopes: None,
                },
            )
            .unwrap();
            assert_eq!(
                resolve_api_key(TEST_PROVIDER, None).as_deref(),
                Some("tid=1;proxy-ep=proxy.individual.githubcopilot.com")
            );
            // 无 OAuth 凭据的 provider 不受影响
            assert!(resolve_api_key("deepseek", None).is_none());
            // 清理，避免残留到后续测试
            remove_auth(TEST_PROVIDER).ok();
        });
    }

    #[test]
    fn api_key_env_var_mapping() {
        assert_eq!(api_key_env_var("deepseek"), Some("DEEPSEEK_API_KEY"));
        assert_eq!(api_key_env_var("opencode"), Some("OPENCODE_API_KEY"));
        assert_eq!(api_key_env_var("opencode-go"), Some("OPENCODE_API_KEY"));
        assert_eq!(api_key_env_var("anthropic"), Some("ANTHROPIC_API_KEY"));
        // 新扩展 provider（对齐 pi envApiKeyAuth）
        assert_eq!(api_key_env_var("openai"), Some("OPENAI_API_KEY"));
        assert_eq!(api_key_env_var("google"), Some("GEMINI_API_KEY"));
        assert_eq!(api_key_env_var("groq"), Some("GROQ_API_KEY"));
        assert_eq!(api_key_env_var("meta"), Some("META_API_KEY"));
        assert_eq!(api_key_env_var("huggingface"), Some("HF_TOKEN"));
        assert_eq!(api_key_env_var("openrouter"), Some("OPENROUTER_API_KEY"));
        assert_eq!(api_key_env_var("xai"), Some("XAI_API_KEY"));
        assert_eq!(
            api_key_env_var("github-copilot"),
            Some("COPILOT_GITHUB_TOKEN")
        );
        assert_eq!(
            api_key_env_var("qwen-token-plan-cn"),
            Some("QWEN_TOKEN_PLAN_CN_API_KEY")
        );
        assert_eq!(
            api_key_env_var("zai-coding-cn"),
            Some("ZAI_CODING_CN_API_KEY")
        );
        assert_eq!(api_key_env_var("typesafe"), Some("TYPESAFE_API_KEY"));
        // 排除的 provider / 未知
        assert_eq!(api_key_env_var("amazon-bedrock"), None);
        assert_eq!(api_key_env_var("mistral"), None);
        assert_eq!(api_key_env_var("openai-codex"), None);
        assert_eq!(api_key_env_var("unknown"), None);
        // 所有支持 API key 的注册表 provider 均有映射
        for p in crate::core::login_registry::LOGIN_PROVIDERS {
            if p.supports_api_key {
                assert!(api_key_env_var(p.id).is_some(), "{} 缺少 env 映射", p.id);
            }
        }
    }

    #[test]
    fn list_configured_providers_includes_env_key() {
        with_temp_agent_dir(|| {
            let _ek = crate::test_support::env_key_lock();
            let _ = std::fs::remove_file(auth_path());
            let _ = std::fs::remove_file(agent_dir().join("models.json"));
            unsafe { std::env::remove_var("DEEPSEEK_API_KEY") };
            assert!(!list_configured_providers().iter().any(|p| p == "deepseek"));

            // 内置 env 变量非空 → provider 纳入（对齐 pi environment 来源）
            unsafe { std::env::set_var("DEEPSEEK_API_KEY", "sk-env") };
            assert!(list_configured_providers().iter().any(|p| p == "deepseek"));
            assert_eq!(env_key_source("deepseek"), Some("DEEPSEEK_API_KEY"));

            // 空值不算已配置
            unsafe { std::env::set_var("DEEPSEEK_API_KEY", "") };
            assert!(env_key_source("deepseek").is_none());
            assert!(!list_configured_providers().iter().any(|p| p == "deepseek"));

            // 已删除 / 未知 provider 无 env 源
            assert!(env_key_source("openai-codex").is_none());
            assert!(env_key_source("unknown").is_none());
            unsafe { std::env::remove_var("DEEPSEEK_API_KEY") };
        });
    }

    #[test]
    fn resolve_api_key_models_json_precedes_env() {
        with_temp_agent_dir(|| {
            let _ek = crate::test_support::env_key_lock();
            // 防御：清除可能被其他测试残留的进程 env
            unsafe {
                std::env::remove_var("DEEPSEEK_API_KEY");
                std::env::remove_var("PRUX_CFG_KEY");
            }
            let _ = std::fs::remove_file(auth_path());
            let _ = std::fs::remove_file(agent_dir().join("models.json"));
            // 无任何来源 → None
            assert!(resolve_api_key("deepseek", None).is_none());

            // 对齐 pi composeApiKeyAuth.resolve：models.json rawKey 优先于内置 provider 的 env 探测
            unsafe { std::env::set_var("DEEPSEEK_API_KEY", "sk-env") };
            std::fs::write(
                agent_dir().join("models.json"),
                r#"{"providers":{"deepseek":{"apiKey":"sk-models-json"}}}"#,
            )
            .unwrap();
            assert_eq!(
                resolve_api_key("deepseek", None).as_deref(),
                Some("sk-models-json"),
                "models.json 字面量应优先于环境变量"
            );

            // 无 models.json 时回退环境变量
            let _ = std::fs::remove_file(agent_dir().join("models.json"));
            assert_eq!(
                resolve_api_key("deepseek", None).as_deref(),
                Some("sk-env"),
                "models.json 缺席时环境变量兜底"
            );

            // models.json 环境变量引用；env 缺失时未定义的行为保持（resolve_config_value 解析）
            unsafe { std::env::set_var("PRUX_CFG_KEY", "sk-via-env") };
            std::fs::write(
                agent_dir().join("models.json"),
                r#"{"providers":{"deepseek":{"apiKey":"$PRUX_CFG_KEY"}}}"#,
            )
            .unwrap();
            assert_eq!(
                resolve_api_key("deepseek", None).as_deref(),
                Some("sk-via-env"),
                "models.json $ENV 引用可解析时生效"
            );

            // 对齐 pi：models.json 声明的 apiKey 引用不可解析时**不回退**环境变量（此时视为未配置）
            std::fs::write(
                agent_dir().join("models.json"),
                r#"{"providers":{"deepseek":{"apiKey":"$PRUX_CFG_MISSING"}}}"#,
            )
            .unwrap();
            assert!(
                resolve_api_key("deepseek", None).is_none(),
                "models.json apiKey 不可解析时应为 None，不得回退 DEEPSEEK_API_KEY"
            );

            // auth.json 凭据优先于 models.json/env
            std::fs::write(
                agent_dir().join("models.json"),
                r#"{"providers":{"deepseek":{"apiKey":"$PRUX_CFG_MISSING"}}}"#,
            )
            .unwrap();
            write_auth_key("deepseek", "sk-stored").unwrap();
            assert_eq!(
                resolve_api_key("deepseek", None).as_deref(),
                Some("sk-stored")
            );
            // --api-key flag 最高
            assert_eq!(
                resolve_api_key("deepseek", Some("sk-flag".to_string())).as_deref(),
                Some("sk-flag")
            );
            unsafe {
                std::env::remove_var("DEEPSEEK_API_KEY");
                std::env::remove_var("PRUX_CFG_KEY");
            }
            let _ = std::fs::remove_file(agent_dir().join("models.json"));
        });
    }

    #[test]
    fn list_configured_providers_merges_models_json() {
        with_temp_agent_dir(|| {
            let _ek = crate::test_support::env_key_lock();
            unsafe { std::env::remove_var("DEEPSEEK_API_KEY") };
            let _ = std::fs::remove_file(auth_path());
            let _ = std::fs::remove_file(agent_dir().join("models.json"));
            assert!(list_configured_providers().is_empty());

            // auth.json 凭据
            write_auth_key("deepseek", "sk-a").unwrap();
            // models.json：字面量 key → 已配置；env 引用缺失 → 未配置
            std::fs::write(
                agent_dir().join("models.json"),
                r#"{"providers":{
                    "my-ollama":{"apiKey":"ollama"},
                    "my-vllm":{"apiKey":"$PRUX_MISSING_KEY"}
                }}"#,
            )
            .unwrap();
            let mut got = list_configured_providers();
            got.sort();
            assert_eq!(got, vec!["deepseek".to_string(), "my-ollama".to_string()]);

            // 命令 apiKey 恒为已配置（不执行）
            std::fs::write(
                agent_dir().join("models.json"),
                r#"{"providers":{"my-cmd":{"apiKey":"!security find-generic-password"}}}"#,
            )
            .unwrap();
            let mut got = list_configured_providers();
            got.sort();
            assert_eq!(got, vec!["deepseek".to_string(), "my-cmd".to_string()]);

            // 对齐 pi check：models.json 声明了 apiKey 且引用不可解析时，
            // 内置环境变量不再是该 provider 的来源
            remove_auth("deepseek").ok();
            unsafe { std::env::set_var("DEEPSEEK_API_KEY", "sk-env") };
            std::fs::write(
                agent_dir().join("models.json"),
                r#"{"providers":{"deepseek":{"apiKey":"$PRUX_MISSING_KEY"}}}"#,
            )
            .unwrap();
            assert!(
                !list_configured_providers().iter().any(|p| p == "deepseek"),
                "models.json 声明了不可解析 apiKey 时不得回退环境变量"
            );
            // 移除该声明 → 环境变量重新成为来源
            let _ = std::fs::remove_file(agent_dir().join("models.json"));
            assert!(list_configured_providers().iter().any(|p| p == "deepseek"));
            unsafe { std::env::remove_var("DEEPSEEK_API_KEY") };

            let _ = std::fs::remove_file(agent_dir().join("models.json"));
        });
    }

    #[test]
    fn auth_cache_reflects_external_edits() {
        with_temp_agent_dir(|| {
            let path = auth_path();
            std::fs::write(&path, r#"{"deepseek":{"type":"api_key","key":"sk-1"}}"#).unwrap();
            assert_eq!(read_auth_key("deepseek").unwrap(), "sk-1");

            // 外部改写 → 缓存失效，读到新 key
            std::fs::write(
                &path,
                r#"{"deepseek":{"type":"api_key","key":"sk-external-2"}}"#,
            )
            .unwrap();
            assert_eq!(read_auth_key("deepseek").unwrap(), "sk-external-2");

            // 删除后按「无凭据」处理
            let _ = std::fs::remove_file(&path);
            assert!(!has_auth("deepseek"));
        });
    }

    #[test]
    fn concurrent_auth_writes_do_not_lose_updates() {
        // auth.json 的读-改-写同样必须串行（并发 /login /logout 不得丢 provider 条目）。
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::test_support::pin_test_agent_dir();
        let _ = std::fs::remove_file(auth_path());

        let n: u64 = 16;
        std::thread::scope(|s| {
            for i in 0..n {
                s.spawn(move || {
                    for _ in 0..20 {
                        write_auth_key(&format!("prov{i}"), &format!("key{i}")).unwrap();
                    }
                });
            }
        });

        let providers = list_auth_providers();
        for i in 0..n {
            let p = format!("prov{i}");
            assert!(
                providers.iter().any(|x| x == &p),
                "并发写入丢失 provider {p}: {providers:?}"
            );
            assert_eq!(read_auth_key(&p).unwrap(), format!("key{i}"));
        }
        let _ = std::fs::remove_file(auth_path());
    }
}
