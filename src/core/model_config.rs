//! models.json 用户配置层
//!
//! 在 `agent_dir()/models.json` 用 JSON 声明第三方 provider 与模型
//! （Ollama / vLLM / LM Studio / 代理等），或给内置 provider 换 `baseUrl`、
//! 合并模型、加 `headers`。文件每次读取实时生效，打开 /model 即重载，无需重启。
//!
//! 合并语义（见 model_resolver::provider_models）：
//! 静态基线 → models-store.json 动态缓存 → models.json（最高优先）。
//!
//! `apiKey` / `headers` 字段支持三种值来源
//! - `!command`：作为 shell 命令执行，取 stdout（trim）
//! - `$ENV_VAR` / `${ENV_VAR}`：环境变量插值；`$$` 转义字面 `$`，`$!` 转义字面 `!`
//! - 字面量

use crate::core::settings_manager::agent_dir;
use serde_json::Value;
use std::{
    collections::HashMap,
    io::Read,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

/// 配置值 shell 命令执行超时
const COMMAND_TIMEOUT_MS: u64 = 10_000;

/// models.json 中某 provider 的配置
#[derive(Debug, Clone, Default)]
pub struct ProviderConfig {
    /// provider 级 baseUrl（覆盖模型目录的 baseUrl）
    pub base_url: Option<String>,
    /// provider 级 api（models 条目缺省时使用）
    pub api: Option<String>,
    /// provider 级 apiKey（原始值，未解析）
    pub api_key: Option<String>,
    /// provider 级 headers（原始值，未解析）
    pub headers: Vec<(String, String)>,
    /// 是否自动加 `Authorization: Bearer <apiKey>`
    pub auth_header: bool,
    /// provider 级 compat（合并进每个模型条目）
    pub compat: Option<Value>,
    /// 自定义模型条目（原始对象）
    pub models: Vec<Value>,
    /// modelOverrides：model id → 覆盖字段
    pub model_overrides: Vec<(String, Value)>,
}

impl ProviderConfig {
    /// apiKey 是否构成「已配置」（/model 可用性检查；不执行 shell 命令）。
    /// 命令恒为已配置；环境变量引用需全部可解析；字面量恒为已配置。
    pub fn has_configured_key(&self) -> bool {
        self.api_key.as_deref().is_some_and(config_value_configured)
    }
}

/// 读取 models.json 原始内容（不存在 / 解析失败返回 None，不报错不生成文件）
fn read_file() -> Option<Value> {
    let path = agent_dir().join("models.json");
    let text = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&text).ok()
}

/// 读取全部 provider 配置。每次调用实时读取：改文件后打开 /model 即生效。
pub fn read_models_config() -> HashMap<String, ProviderConfig> {
    let mut out = HashMap::new();
    let Some(root) = read_file() else {
        return out;
    };
    let Some(providers) = root.get("providers").and_then(|v| v.as_object()) else {
        return out;
    };
    let entries: Vec<(String, &Value)> = providers.iter().map(|(k, v)| (k.clone(), v)).collect();
    for (id, p) in entries {
        out.insert(id, parse_provider(p));
    }
    out
}

/// 读取某 provider 的配置（不存在返回 None）
pub fn provider_config(provider: &str) -> Option<ProviderConfig> {
    read_models_config().remove(provider)
}

/// 取 JSON 对象的字符串字段（非字符串 / 缺失返回 `None`）。
fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(|s| s.to_string())
}

/// 把 JSON 对象形式的 headers 转成键值对（非字符串值丢弃）。
fn headers_from(v: Option<&Value>) -> Vec<(String, String)> {
    v.and_then(|h| h.as_object())
        .map(|o| {
            o.iter()
                .filter_map(|(k, x)| x.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

/// 把单个 provider 的 JSON 对象解析成 [`ProviderConfig`]（缺失字段取缺省）。
fn parse_provider(p: &Value) -> ProviderConfig {
    ProviderConfig {
        base_url: str_field(p, "baseUrl"),
        api: str_field(p, "api"),
        api_key: str_field(p, "apiKey"),
        headers: headers_from(p.get("headers")),
        auth_header: p
            .get("authHeader")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        compat: p.get("compat").cloned(),
        models: p
            .get("models")
            .and_then(|m| m.as_array())
            .cloned()
            .unwrap_or_default(),
        model_overrides: p
            .get("modelOverrides")
            .and_then(|v| v.as_object())
            .map(|o| o.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default(),
    }
}

/// 配置值模板解析出的片段：待展开的环境变量名或原样保留的字面量。
#[derive(Debug, Clone, PartialEq)]
enum TemplatePart {
    /// 原样保留的文本片段，不展开。
    Literal(String),
    /// 待展开的环境变量名（如 `HOME`）。
    Env(String),
}

/// 字节是否可作为环境变量名首字符（字母或下划线）。
fn is_env_name_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_'
}

/// 字节是否可作为环境变量名后续字符（字母、数字或下划线）。
fn is_env_name_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// 字符串是否是合法的环境变量名（非空、首字符合法、其余字符合法）。
fn is_valid_env_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && is_env_name_start(bytes[0])
        && bytes[1..].iter().all(|c| is_env_name_char(*c))
}

/// 解析配置值模板：`$ENV` / `${ENV}` → 环境变量；`$$` → 字面 `$`；`$!` → 字面 `!`；
/// 其余为字面量。
fn parse_template(config: &str) -> Vec<TemplatePart> {
    let bytes = config.as_bytes();
    let mut parts: Vec<TemplatePart> = Vec::new();
    let mut literal = String::new();
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index] != b'$' {
            let start = index;
            while index < bytes.len() && bytes[index] != b'$' {
                index += 1;
            }
            literal.push_str(&config[start..index]);
            continue;
        }

        match bytes.get(index + 1).copied() {
            Some(b'$') | Some(b'!') => {
                literal.push(bytes[index + 1] as char);
                index += 2;
            }
            Some(b'{') => {
                if let Some(close) = config[index + 2..].find('}') {
                    let name = &config[index + 2..index + 2 + close];
                    if is_valid_env_name(name) {
                        if !literal.is_empty() {
                            parts.push(TemplatePart::Literal(std::mem::take(&mut literal)));
                        }
                        parts.push(TemplatePart::Env(name.to_string()));
                        index += 2 + close + 1;
                    } else {
                        literal.push('$');
                        index += 1;
                    }
                } else {
                    literal.push('$');
                    index += 1;
                }
            }
            Some(c) if is_env_name_start(c) => {
                let mut end = index + 2;
                while end < bytes.len() && is_env_name_char(bytes[end]) {
                    end += 1;
                }
                let name = &config[index + 1..end];
                if !literal.is_empty() {
                    parts.push(TemplatePart::Literal(std::mem::take(&mut literal)));
                }
                parts.push(TemplatePart::Env(name.to_string()));
                index = end;
            }
            _ => {
                literal.push('$');
                index += 1;
            }
        }
    }

    if !literal.is_empty() {
        parts.push(TemplatePart::Literal(literal));
    }
    parts
}

/// 配置值引用的环境变量名（无引用返回空）
pub fn config_value_env_var_names(config: &str) -> Vec<String> {
    if config.starts_with('!') {
        return Vec::new();
    }

    let mut names = Vec::new();
    for part in parse_template(config) {
        if let TemplatePart::Env(name) = part
            && !names.contains(&name)
        {
            names.push(name);
        }
    }
    names
}

/// 配置值是否构成「已配置」：命令恒为真；环境变量引用需全部可解析；否则为真。
/// 用于 /model 可用性检查，**不执行 shell 命令**。
pub fn config_value_configured(config: &str) -> bool {
    if config.starts_with('!') {
        return true;
    }

    parse_template(config).iter().all(|part| match part {
        TemplatePart::Literal(_) => true,
        TemplatePart::Env(name) => std::env::var(name).is_ok(),
    })
}

/// 执行 shell 命令并取 stdout（trim；失败 / 非零退出 / 超时返回 None）。
fn run_command(command: &str) -> Option<String> {
    #[cfg(unix)]
    let mut cmd = {
        let mut c = Command::new("sh");
        c.arg("-c").arg(command);
        c
    };

    #[cfg(windows)]
    let mut cmd = {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(command);
        c
    };

    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let deadline = Instant::now() + Duration::from_millis(COMMAND_TIMEOUT_MS);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    _ = child.kill();
                    _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => return None,
        }
    };

    if !status.success() {
        return None;
    }

    let mut stdout = String::new();
    _ = child
        .stdout
        .take()
        .and_then(|mut o| o.read_to_string(&mut stdout).ok());
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// 解析配置值：
/// - `!command` 开头 → 执行命令取 stdout
/// - 否则解析 `$ENV` / `${ENV}` 插值（缺失的环境变量使整体无法解析 → None）
/// - `$$` → 字面 `$`；`$!` → 字面 `!`
pub fn resolve_config_value(config: &str) -> Option<String> {
    if let Some(command) = config.strip_prefix('!') {
        return run_command(command);
    }
    let mut out = String::new();
    for part in parse_template(config) {
        match part {
            TemplatePart::Literal(s) => out.push_str(&s),
            TemplatePart::Env(name) => out.push_str(&std::env::var(&name).ok()?),
        }
    }
    Some(out)
}

/// 解析 headers（值解析；解析失败的条目跳过）
pub fn resolve_headers(headers: &[(String, String)]) -> Vec<(String, String)> {
    headers
        .iter()
        .filter_map(|(k, v)| resolve_config_value(v).map(|val| (k.clone(), val)))
        .collect()
}

/// models.json 中某模型的请求头（provider 级在前，model 级在后；值解析后）。
/// model 级来源：modelOverrides 的 headers → models 条目自身的 headers（后者覆盖）。
/// 解析失败的条目跳过
pub fn configured_request_headers(provider: &str, model_id: &str) -> Vec<(String, String)> {
    let Some(cfg) = provider_config(provider) else {
        return Vec::new();
    };
    let mut headers: Vec<(String, String)> = cfg.headers;
    if let Some((_, ov)) = cfg.model_overrides.iter().find(|(id, _)| id == model_id) {
        headers.extend(headers_from(ov.get("headers")));
    }
    if let Some(m) = cfg
        .models
        .iter()
        .find(|m| m.get("id").and_then(|x| x.as_str()) == Some(model_id))
    {
        headers.extend(headers_from(m.get("headers")));
    }
    resolve_headers(&headers)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn test_dir() -> std::path::PathBuf {
        // 线程本地 AgentDirGuard 已隔离；返回当前 agent_dir 供各测试落盘
        crate::core::settings_manager::agent_dir()
    }

    fn write_models_json(v: serde_json::Value) {
        let path = agent_dir().join("models.json");
        std::fs::write(&path, serde_json::to_string(&v).unwrap()).unwrap();
    }

    #[test]
    fn parse_template_handles_env_and_escapes() {
        // $ENV 贪婪匹配变量名
        let parts = parse_template("$FOO_BAR");
        assert_eq!(parts, vec![TemplatePart::Env("FOO_BAR".into())]);
        // ${ENV} 限定边界，后续字面量
        let parts = parse_template("${FOO}_BAR");
        assert_eq!(
            parts,
            vec![
                TemplatePart::Env("FOO".into()),
                TemplatePart::Literal("_BAR".into())
            ]
        );
        // $$ → 字面 $；$! → 字面 !（不触发命令）
        let parts = parse_template("$$literal-$!bang");
        assert_eq!(parts, vec![TemplatePart::Literal("$literal-!bang".into())]);
        // 未知 $ 结尾 / 非法 ${} 按字面
        let parts = parse_template("a$  b${");
        assert_eq!(parts, vec![TemplatePart::Literal("a$  b${".into())]);
        // 无 $ 纯字面
        assert_eq!(
            parse_template("plain"),
            vec![TemplatePart::Literal("plain".into())]
        );
    }

    #[test]
    fn resolve_config_value_env_and_literal() {
        let _ek = crate::test_support::env_key_lock();
        // PRUX_TEST_KEY 是进程级环境变量，与 config_value_configured_without_executing
        // 并行 set/remove 同名变量会互踩：持锁串行。
        let _ad = crate::test_support::AgentDirGuard::temp();
        unsafe {
            std::env::set_var("PRUX_TEST_KEY", "sk-env-value");
        }
        // 单环境变量
        assert_eq!(
            resolve_config_value("$PRUX_TEST_KEY").as_deref(),
            Some("sk-env-value")
        );
        // 插值拼接
        assert_eq!(
            resolve_config_value("${PRUX_TEST_KEY}_suffix").as_deref(),
            Some("sk-env-value_suffix")
        );
        // 字面量
        assert_eq!(
            resolve_config_value("sk-literal").as_deref(),
            Some("sk-literal")
        );
        // 转义
        assert_eq!(
            resolve_config_value("$$dollar-$!bang").as_deref(),
            Some("$dollar-!bang")
        );
        // 缺失环境变量 → None
        assert!(resolve_config_value("$PRUX_TEST_MISSING_VAR").is_none());
        assert!(resolve_config_value("${PRUX_TEST_KEY}_${PRUX_TEST_MISSING_VAR}").is_none());
        unsafe {
            std::env::remove_var("PRUX_TEST_KEY");
        }
    }

    #[test]
    fn resolve_config_value_runs_command() {
        // 命令取 stdout（trim）
        assert_eq!(
            resolve_config_value("!printf '  key-from-command  '").as_deref(),
            Some("key-from-command")
        );
        // 非零退出 → None
        assert!(resolve_config_value("!exit 3").is_none());
        // 空输出 → None
        assert!(resolve_config_value("!true").is_none());
        // 命令形式不解析内部 $（整体作为命令字符串）
        assert_eq!(
            resolve_config_value("!printf '%s' '$HOME'").as_deref(),
            Some("$HOME")
        );
    }

    #[test]
    fn config_value_configured_without_executing() {
        let _ek = crate::test_support::env_key_lock();
        // 与 resolve_config_value_env_and_literal 共用 PRUX_TEST_KEY：持锁串行。
        let _ad = crate::test_support::AgentDirGuard::temp();
        // 命令恒为已配置（不执行）
        assert!(config_value_configured("!op read oops"));
        // 字面量恒为已配置
        assert!(config_value_configured("sk-literal"));
        // 环境变量引用需可解析
        unsafe {
            std::env::set_var("PRUX_TEST_KEY", "x");
        }
        assert!(config_value_configured("$PRUX_TEST_KEY"));
        assert!(config_value_configured("${PRUX_TEST_KEY}_x"));
        unsafe {
            std::env::remove_var("PRUX_TEST_KEY");
        }
        assert!(!config_value_configured("$PRUX_TEST_KEY"));
        assert!(!config_value_configured("${PRUX_TEST_KEY}_x"));
        // 多引用：一个缺失即未配置
        unsafe {
            std::env::set_var("PRUX_TEST_KEY", "x");
        }
        assert!(!config_value_configured(
            "${PRUX_TEST_KEY}_${PRUX_TEST_MISSING}"
        ));
        assert!(config_value_configured("${PRUX_TEST_KEY}_${PRUX_TEST_KEY}"));
        unsafe {
            std::env::remove_var("PRUX_TEST_KEY");
        }
    }

    #[test]
    fn read_and_parse_provider_config() {
        let _ek = crate::test_support::env_key_lock();
        let _ad = crate::test_support::AgentDirGuard::temp();
        test_dir();
        let _ = std::fs::remove_file(agent_dir().join("models.json"));
        // 无文件 → 空配置
        assert!(read_models_config().is_empty());

        write_models_json(serde_json::json!({
            "providers": {
                "my-ollama": {
                    "baseUrl": "http://localhost:11434/v1",
                    "api": "openai-completions",
                    "apiKey": "ollama",
                    "authHeader": true,
                    "headers": { "x-custom": "$PRUX_TEST_HEADER" },
                    "compat": { "supportsDeveloperRole": false },
                    "models": [
                        { "id": "llama3.1:8b" },
                        {
                            "id": "qwen2.5-coder:7b",
                            "name": "Qwen Coder 本地版",
                            "reasoning": false,
                            "input": ["text"],
                            "contextWindow": 128000,
                            "maxTokens": 32000
                        }
                    ],
                    "modelOverrides": {
                        "llama3.1:8b": {
                            "name": "Llama 本地",
                            "headers": { "x-model": "llama" }
                        }
                    }
                }
            }
        }));

        let cfg = read_models_config();
        let ollama = cfg.get("my-ollama").unwrap();
        assert_eq!(
            ollama.base_url.as_deref(),
            Some("http://localhost:11434/v1")
        );
        assert_eq!(ollama.api.as_deref(), Some("openai-completions"));
        assert_eq!(ollama.api_key.as_deref(), Some("ollama"));
        assert!(ollama.auth_header);
        assert_eq!(
            ollama.headers,
            vec![("x-custom".to_string(), "$PRUX_TEST_HEADER".to_string())]
        );
        assert_eq!(ollama.models.len(), 2);
        assert_eq!(ollama.model_overrides.len(), 1);
        assert!(ollama.has_configured_key());

        // 值解析后的请求头：provider 级 + model 级覆盖
        unsafe {
            std::env::set_var("PRUX_TEST_HEADER", "hdr-value");
        }
        let h = configured_request_headers("my-ollama", "llama3.1:8b");
        let map: std::collections::HashMap<String, String> = h.into_iter().collect();
        assert_eq!(map.get("x-custom").map(|s| s.as_str()), Some("hdr-value"));
        assert_eq!(map.get("x-model").map(|s| s.as_str()), Some("llama"));
        // 模型级 headers 只对声明它的模型生效
        let h2 = configured_request_headers("my-ollama", "qwen2.5-coder:7b");
        let map2: std::collections::HashMap<String, String> = h2.into_iter().collect();
        assert_eq!(map2.get("x-custom").map(|s| s.as_str()), Some("hdr-value"));
        assert!(!map2.contains_key("x-model"));
        unsafe {
            std::env::remove_var("PRUX_TEST_HEADER");
        }
        // 未知 provider 无头
        assert!(configured_request_headers("nope", "x").is_empty());
        let _ = std::fs::remove_file(agent_dir().join("models.json"));
    }
}
