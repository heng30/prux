//! `auth` 子命令：打印凭据或检查 provider 认证就绪状态。
//!
//! - `prux auth print-api-key [--provider <provider>] [--model <model>]`
//! - `prux auth print-bearer-token [--provider <provider>] [--model <model>] [--min-expiry <duration>]`
//! - `prux auth check [--provider <provider>] [--model <model>] [--json] [--credentials] [--no-refresh]`
//!
//! 凭据来源与运行时一致：`auth.json`（type=api_key / oauth，见 settings_manager）。
//! prux 尚未实现 OAuth 刷新：oauth 凭据只读 access 令牌，不执行刷新。

use crate::{
    core::{
        auth::{ensure_oauth_valid, list_auth_types, read_auth_entry, read_oauth_credential},
        model_resolver::{find_model, is_known_provider, parse_model_arg},
    },
    utils::time::now_ms,
};
use serde_json::json;

/// auth 子命令种类
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthCommandKind {
    /// `auth check`：检查认证就绪状态（exit code 表示结果）
    Check,
    /// `auth print-api-key`：打印解析后的 API key
    ApiKey,
    /// `auth print-bearer-token`：打印 OAuth bearer token（可要求最小剩余有效期）
    BearerToken,
}

impl AuthCommandKind {
    /// 用于错误文案的子命令名（如 `auth check`）
    fn name(&self) -> &'static str {
        match self {
            AuthCommandKind::Check => "auth check",
            AuthCommandKind::ApiKey => "auth print-api-key",
            AuthCommandKind::BearerToken => "auth print-bearer-token",
        }
    }

    /// 该子命令的用法行（错误提示中展示）
    fn usage(&self) -> &'static str {
        match self {
            AuthCommandKind::Check => {
                "prux auth check --provider <provider> [--json] [--credentials] [--no-refresh]"
            }
            AuthCommandKind::ApiKey => {
                "prux auth print-api-key --provider <provider> [--model <model>]"
            }
            AuthCommandKind::BearerToken => {
                "prux auth print-bearer-token --provider <provider> [--model <model>] [--min-expiry <duration>]"
            }
        }
    }
}

/// 解析后的 auth 命令
#[derive(Debug, Clone)]
pub struct AuthCommand {
    /// 子命令种类
    pub kind: AuthCommandKind,
    /// 目标 provider（与 `model` 至少给一个）
    pub provider: Option<String>,
    /// 目标模型（可带 provider 前缀，用于反推 provider）
    pub model: Option<String>,
    /// 仅 check
    pub json: bool,
    /// 仅 check
    pub credentials: bool,
    /// 仅 check：禁止刷新过期凭据
    pub no_refresh: bool,
    /// 仅 print-bearer-token：最小剩余有效期（毫秒）
    pub min_expiry_ms: Option<u64>,
}

/// auth 用法帮助
pub fn print_auth_command_help() {
    println!(
        "Usage:\n  prux auth print-api-key [--provider <provider>] [--model <model>]\n  prux auth print-bearer-token [--provider <provider>] [--model <model>] [--min-expiry <duration>]\n  prux auth check [--provider <provider>] [--model <model>] [--json] [--credentials] [--no-refresh]\n\nAuth commands require at least one of --provider or --model. Checks refresh expired OAuth credentials by default; --no-refresh prevents this. --credentials emits the credential, or includes it in JSON output."
    );
}

/// 解析 auth 子命令参数
/// 参数为 clap `Command::Auth { args }` 收集的剩余参数，不含 "auth" 本身
/// （args[0] 为子命令名，如 "check"）。
pub fn parse_auth_command(args: &[String]) -> Result<AuthCommand, String> {
    let kind = match args.first().map(String::as_str) {
        Some("check") => AuthCommandKind::Check,
        Some("print-api-key") => AuthCommandKind::ApiKey,
        Some("print-bearer-token") => AuthCommandKind::BearerToken,
        Some(other) => {
            return Err(format!(
                "Unknown auth command \"{}\". Use \"prux auth print-api-key\", \"prux auth print-bearer-token\", or \"prux auth check\".",
                other
            ));
        }
        None => {
            return Err("auth requires a subcommand. Use \"prux auth print-api-key\", \"prux auth print-bearer-token\", or \"prux auth check\".".to_string());
        }
    };

    let mut provider: Option<String> = None;
    let mut model: Option<String> = None;
    let mut json = false;
    let mut credentials = false;
    let mut no_refresh = false;
    let mut min_expiry_ms: Option<u64> = None;

    let mut i = 1;
    while i < args.len() {
        let arg = args[i].as_str();
        match arg {
            "--provider" => {
                i += 1;
                provider = args.get(i).cloned().filter(|s| !s.is_empty());
                if provider.is_none() {
                    return Err(format!(
                        "Missing value for --provider for \"{}\".",
                        kind.name()
                    ));
                }
            }
            "--model" => {
                i += 1;
                model = args.get(i).cloned().filter(|s| !s.is_empty());
                if model.is_none() {
                    return Err(format!(
                        "Missing value for --model for \"{}\".",
                        kind.name()
                    ));
                }
            }
            "--min-expiry" => {
                if kind != AuthCommandKind::BearerToken {
                    return Err("--min-expiry is only supported by print-bearer-token".to_string());
                }
                i += 1;
                let value = args.get(i).map(String::as_str).unwrap_or("");
                min_expiry_ms = Some(parse_duration_ms(value).ok_or_else(|| {
                    "--min-expiry must use a duration such as 30m or 1h".to_string()
                })?);
            }
            "--json" | "--credentials" | "--no-refresh" => {
                if kind != AuthCommandKind::Check {
                    return Err(format!("{} is only supported by auth check", arg));
                }
                match arg {
                    "--json" => json = true,
                    "--credentials" => credentials = true,
                    _ => no_refresh = true,
                }
            }
            "--help" | "-h" => {
                print_auth_command_help();
                std::process::exit(0);
            }
            other if other.starts_with("--") || (other.starts_with('-') && other.len() > 1) => {
                return Err(format!(
                    "Unknown option {} for \"{}\".\nUse \"prux --help\" or \"{}\".",
                    other,
                    kind.name(),
                    kind.usage()
                ));
            }
            other => {
                return Err(format!(
                    "Unexpected argument \"{}\" for \"{}\". Use \"prux auth print-api-key\", \"prux auth print-bearer-token\", or \"prux auth check\".",
                    other,
                    kind.name()
                ));
            }
        }
        i += 1;
    }

    Ok(AuthCommand {
        kind,
        provider,
        model,
        json,
        credentials,
        no_refresh,
        min_expiry_ms,
    })
}

/// 解析 duration（对齐 pi：`(\d+)(ms|s|m|h)`，不区分大小写）
fn parse_duration_ms(value: &str) -> Option<u64> {
    let (num, unit) = value
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_ascii_digit())
        .map(|(idx, _)| value.split_at(idx + 1))
        .unwrap_or(("", ""));
    let amount: u64 = num.parse().ok()?;
    let ms = match unit.to_ascii_lowercase().as_str() {
        "ms" => 1,
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        _ => return None,
    };
    Some(amount * ms)
}

/// 校验 auth 命令参数：至少提供 --provider 或 --model（对齐 pi validateAuthCommandArgs）
fn validate_provider_model(
    kind: AuthCommandKind,
    provider: Option<&str>,
    model: Option<&str>,
) -> Result<(Option<String>, Option<String>), String> {
    let provider = provider
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let model = model
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    if provider.is_none() && model.is_none() {
        return Err(match kind {
            AuthCommandKind::Check => {
                "Auth checks require --provider <provider> or --model <model>".to_string()
            }
            _ => {
                "Credential printing requires --provider <provider> or --model <model>".to_string()
            }
        });
    }
    Ok((provider, model))
}

/// check 结果
#[derive(Debug, Clone)]
struct CheckResult {
    /// `ready` / `not_ready` / 非法状态（决定 exit code 0/1/2）
    status: &'static str,
    /// 实际检查的 provider
    provider: String,
    /// not_ready 的原因码（如 provider_not_found）
    reason: Option<&'static str>,
    /// 命中的凭据类型（api_key / oauth）
    auth_type: Option<&'static str>,
    /// 凭据本体（仅 --credentials 时填充）
    credential: Option<String>,
}

/// 执行 `auth check`
fn run_check(cmd: &AuthCommand) -> i32 {
    let (provider, model) =
        match validate_provider_model(cmd.kind, cmd.provider.as_deref(), cmd.model.as_deref()) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("Error: {}", e);
                return 2;
            }
        };

    // check 内部错误归为 invalid/invalid_state（含 model 解析失败、凭据读取异常）
    let mut result = match resolve_provider_for_check(provider.as_deref(), model.as_deref()) {
        Ok(provider) => check_provider_auth(&provider, cmd.no_refresh),
        Err(e) => {
            eprintln!("Error: {}", e);
            return 2;
        }
    };

    // --credentials：ready 时附带凭据。oauth 分支已带刷新后的 token，仅在缺省时读 entry；取不到则降级为 not_ready。
    if cmd.credentials && result.status == "ready" && result.credential.is_none() {
        match read_auth_entry(&result.provider) {
            Some((_, token)) => result.credential = Some(token),
            None => {
                result = CheckResult {
                    status: "not_ready",
                    provider: result.provider.clone(),
                    reason: Some("credential_not_available"),
                    auth_type: None,
                    credential: None,
                };
            }
        }
    }

    let output = if cmd.json {
        let mut obj = serde_json::Map::new();
        obj.insert("status".to_string(), json!(result.status));
        obj.insert("provider".to_string(), json!(result.provider));
        if let Some(r) = result.reason {
            obj.insert("reason".to_string(), json!(r));
        }
        if let Some(t) = result.auth_type {
            obj.insert("authType".to_string(), json!(t));
        }
        // 只有 --credentials 时 JSON 才包含 credentials 字段
        if cmd.credentials
            && let Some(c) = &result.credential
        {
            obj.insert("credentials".to_string(), json!(c));
        }
        serde_json::Value::Object(obj).to_string()
    } else if cmd.credentials {
        // 只有 --credentials 时输出凭据，否则输出 status
        result
            .credential
            .clone()
            .unwrap_or_else(|| result.status.to_string())
    } else {
        result.status.to_string()
    };
    println!("{}", output);

    match result.status {
        "ready" => 0,
        "not_ready" => 1,
        _ => 2,
    }
}

/// 解析 check 的 provider：--provider 优先，否则从 --model 推导
fn resolve_provider_for_check(
    provider: Option<&str>,
    model: Option<&str>,
) -> Result<String, String> {
    let mut resolved_provider = provider.map(str::to_string);
    if let Some(m) = model {
        let (mp, _, _) = parse_model_arg(m);
        match (resolved_provider.as_deref(), mp) {
            (Some(p), Some(mp)) if p != mp => {
                return Err(format!("provider \"{}\" does not match model \"{}\"", p, m));
            }
            (_, Some(mp)) => resolved_provider = Some(mp),
            (None, None) => {
                // model 未带 provider 前缀：需要从环境中推导
                if let Some(p) = std::env::var("PRUX_PROVIDER")
                    .ok()
                    .filter(|s| !s.is_empty())
                {
                    resolved_provider = Some(p);
                }
            }
            _ => {}
        }
        if resolved_provider.is_none() {
            return Err(format!("Unable to resolve model \"{}\"", m));
        }
    }
    resolved_provider.ok_or_else(|| "Unable to resolve an auth provider".to_string())
}

/// 检查 provider 认证状态
fn check_provider_auth(provider: &str, no_refresh: bool) -> CheckResult {
    if !is_known_provider(provider) {
        return CheckResult {
            status: "not_ready",
            provider: provider.to_string(),
            reason: Some("provider_not_found"),
            auth_type: None,
            credential: None,
        };
    }
    let Some((cred_type, token)) = read_auth_entry(provider) else {
        return CheckResult {
            status: "not_ready",
            provider: provider.to_string(),
            reason: Some("credentials_not_configured"),
            auth_type: None,
            credential: None,
        };
    };
    match cred_type.as_str() {
        "api_key" => CheckResult {
            status: "ready",
            provider: provider.to_string(),
            reason: None,
            auth_type: Some("api_key"),
            credential: Some(token),
        },
        "oauth" => {
            // 默认对过期 OAuth 实际刷新验证（--no-refresh 跳过）。
            // ensure_oauth_valid 对未过期/permanent/不可刷新凭据不发网络请求；
            // 过期可刷新则真实刷新；刷新失败/解析不出 → not_ready。
            let checked = if no_refresh {
                if !token.is_empty() { Some(token) } else { None }
            } else {
                let rt = match tokio::runtime::Runtime::new() {
                    Ok(rt) => rt,
                    Err(_) => {
                        return CheckResult {
                            status: "not_ready",
                            provider: provider.to_string(),
                            reason: Some("oauth_refresh_failed"),
                            auth_type: Some("oauth"),
                            credential: None,
                        };
                    }
                };
                match rt.block_on(ensure_oauth_valid(provider)) {
                    Ok(opt) => opt,
                    Err(_) => {
                        return CheckResult {
                            status: "not_ready",
                            provider: provider.to_string(),
                            reason: Some("oauth_refresh_failed"),
                            auth_type: Some("oauth"),
                            credential: None,
                        };
                    }
                }
            };
            match checked {
                Some(t) if !t.is_empty() => CheckResult {
                    status: "ready",
                    provider: provider.to_string(),
                    reason: None,
                    auth_type: Some("oauth"),
                    credential: Some(t),
                },
                _ => CheckResult {
                    status: "not_ready",
                    provider: provider.to_string(),
                    reason: Some("credentials_not_configured"),
                    auth_type: None,
                    credential: None,
                },
            }
        }
        _ => CheckResult {
            status: "not_ready",
            provider: provider.to_string(),
            reason: Some("credentials_not_configured"),
            auth_type: None,
            credential: None,
        },
    }
}

/// 执行 `auth print-api-key` / `auth print-bearer-token`
fn run_print(cmd: &AuthCommand) -> i32 {
    let (provider, model) =
        match validate_provider_model(cmd.kind, cmd.provider.as_deref(), cmd.model.as_deref()) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("Error: {}", e);
                return 1;
            }
        };

    let credential =
        match resolve_credential_for_print(cmd.kind, provider.as_deref(), model.as_deref()) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("Error: {}", e);
                return 1;
            }
        };

    // bearer-token 且剩余有效期 < min-expiry 时强制刷新
    if cmd.kind == AuthCommandKind::BearerToken && cmd.min_expiry_ms.is_some() {
        let need_refresh = provider
            .as_deref()
            .and_then(read_oauth_credential)
            .map(|c| {
                !c.is_permanent()
                    && c.expires.saturating_sub(now_ms()) < cmd.min_expiry_ms.unwrap_or(0)
            })
            .unwrap_or(false);

        if need_refresh {
            let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string());
            match rt {
                Err(e) => {
                    eprintln!("Error: failed to start runtime for OAuth refresh: {}", e);
                    return 1;
                }
                Ok(rt) => {
                    match rt.block_on(ensure_oauth_valid(provider.as_deref().unwrap_or(""))) {
                        Ok(Some(t)) => {
                            println!("{}", t);
                            return 0;
                        }
                        Ok(None) => {
                            eprintln!("Error: no OAuth credential to refresh for --min-expiry");
                            return 1;
                        }
                        Err(e) => {
                            eprintln!("Error: {}", e);
                            return 1;
                        }
                    }
                }
            }
        }
    }
    println!("{}", credential);
    0
}

/// 解析并打印单个凭据
fn resolve_credential_for_print(
    kind: AuthCommandKind,
    provider: Option<&str>,
    model: Option<&str>,
) -> Result<String, String> {
    let credential_types: Vec<(String, String)> = list_auth_types();

    // 确定候选 provider：显式 --provider，或遍历所有有凭据的已知 provider
    let candidates: Vec<(String, Option<String>)> = if let Some(p) = provider {
        if !is_known_provider(p) {
            return Err(format!(
                "Unknown provider \"{}\". Use --list-models to see available providers.",
                p
            ));
        }
        if let Some(m) = model {
            let (mp, ..) = parse_model_arg(m);
            if let Some(mp) = mp
                && mp != p
            {
                return Err(format!("provider \"{}\" does not match model \"{}\"", p, m));
            }
            // 校验模型确实存在（对齐 pi resolveCliModel 失败即报错）
            find_model(p, model_name(m)).map_err(|e| e.to_string())?;
            vec![(p.to_string(), Some(m.to_string()))]
        } else {
            vec![(p.to_string(), None)]
        }
    } else {
        let mut out = Vec::new();
        for (pid, _) in &credential_types {
            if !is_known_provider(pid) {
                continue;
            }
            if let Some(m) = model {
                let (mp, _, _) = parse_model_arg(m);
                let ok = match mp {
                    Some(mp) => mp == *pid,
                    None => find_model(pid, model_name(m)).is_ok(),
                };
                if ok {
                    out.push((pid.clone(), Some(m.to_string())));
                }
            } else {
                out.push((pid.clone(), None));
            }
        }
        if out.is_empty() {
            return Err(match model {
                Some(m) => format!(
                    "Model \"{}\" not found. Use --list-models to see available models.",
                    m
                ),
                None => "No configured providers with credentials. Use /login to add providers."
                    .to_string(),
            });
        }
        out
    };

    // 按凭据类型过滤并取值（对齐 pi：api_key 跳过 oauth；bearer_token 只要 oauth）
    let mut credentials: Vec<(String, String)> = Vec::new();
    for (pid, _) in &candidates {
        let Some((cred_type, token)) = read_auth_entry(pid) else {
            continue;
        };
        let want_oauth = kind == AuthCommandKind::BearerToken;
        let matches = if want_oauth {
            cred_type == "oauth"
        } else {
            cred_type != "oauth"
        };
        if matches {
            credentials.push((pid.clone(), token));
        }
    }

    match credentials.len() {
        1 => Ok(credentials.into_iter().next().unwrap().1),
        0 => {
            let pid = candidates.first().map(|(p, _)| p.clone());
            let type_mismatch = |expected: &str| {
                pid.as_ref()
                    .and_then(|p| read_auth_entry(p))
                    .map(|(t, _)| t != expected)
                    .unwrap_or(false)
            };
            if let Some(p) = &pid
                && kind == AuthCommandKind::ApiKey
                && type_mismatch("oauth")
            {
                return Err(format!(
                    "Provider \"{}\" is configured with OAuth, not an API key",
                    p
                ));
            }
            if let Some(p) = &pid
                && kind == AuthCommandKind::BearerToken
                && type_mismatch("oauth")
            {
                return Err(format!(
                    "Provider \"{}\" is not configured with an OAuth bearer token",
                    p
                ));
            }
            Err(if kind == AuthCommandKind::ApiKey {
                "No usable API key is configured".to_string()
            } else {
                "No usable OAuth bearer token is configured".to_string()
            })
        }
        _ => Err(format!(
            "Multiple configured providers matched ({}). Specify --provider.",
            credentials
                .iter()
                .map(|(p, _)| p.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// 提取 --model 中纯模型 id（去掉 provider/ 前缀与 :thinking 后缀）
fn model_name(model: &str) -> &str {
    let after_slash = model.rsplit_once('/').map(|(_, m)| m).unwrap_or(model);
    after_slash
        .split_once(':')
        .map(|(m, _)| m)
        .unwrap_or(after_slash)
}

/// 执行 auth 子命令并返回退出码
pub fn run_auth_command(args: &[String]) -> i32 {
    if args.is_empty()
        || args.first().map(String::as_str) == Some("help")
        || args.iter().any(|a| a == "--help" || a == "-h")
    {
        print_auth_command_help();
        return 0;
    }

    let cmd = match parse_auth_command(args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error: {}", e);
            return 1;
        }
    };

    match cmd.kind {
        AuthCommandKind::Check => run_check(&cmd),
        AuthCommandKind::ApiKey | AuthCommandKind::BearerToken => run_print(&cmd),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::auth::{remove_auth, write_auth_key, write_oauth_credential};
    use crate::core::oauth::{OAuthCredential, simple_credential};
    use crate::test_support::AUTH_TEST_LOCK;

    /// PRUX_AGENT_DIR 可能被其他模块测试劫持：每次调用强制指向共享测试目录
    fn test_dir() {
        let dir = std::env::temp_dir().join("prux-auth-tests");
        let _ = std::fs::create_dir_all(&dir);
        unsafe { std::env::set_var("PRUX_AGENT_DIR", &dir) };
    }

    /// 清空 auth.json 残留凭据：并行测试 panic 可能留下脏条目
    /// （opencode/opencode-go 等由其他模块写入），会让无 --provider 的
    /// resolve_credential_for_print 误报 “Multiple configured providers matched”。
    fn reset_auth() {
        for p in ["deepseek", "opencode", "opencode-go", "anthropic"] {
            remove_auth(p).ok();
        }
    }

    /// 把 `&[&str]` 形式的参数（不含 "auth"）交给 [`parse_auth_command`]（测试便利包装）。
    fn parse(args: &[&str]) -> Result<AuthCommand, String> {
        let full: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        parse_auth_command(&full)
    }

    #[test]
    fn parses_print_api_key() {
        let c = parse(&["print-api-key", "--provider", "deepseek"]).unwrap();
        assert_eq!(c.kind, AuthCommandKind::ApiKey);
        assert_eq!(c.provider.as_deref(), Some("deepseek"));
        assert!(c.model.is_none());
    }

    #[test]
    fn parses_check_flags() {
        let c = parse(&[
            "check",
            "--provider",
            "opencode",
            "--json",
            "--credentials",
            "--no-refresh",
        ])
        .unwrap();
        assert_eq!(c.kind, AuthCommandKind::Check);
        assert!(c.json && c.credentials && c.no_refresh);
        assert_eq!(c.provider.as_deref(), Some("opencode"));
    }

    #[test]
    fn parses_min_expiry() {
        let c = parse(&[
            "print-bearer-token",
            "--provider",
            "deepseek",
            "--min-expiry",
            "30m",
        ])
        .unwrap();
        assert_eq!(c.min_expiry_ms, Some(30 * 60_000));
        let c = parse(&[
            "print-bearer-token",
            "--provider",
            "deepseek",
            "--min-expiry",
            "1h",
        ])
        .unwrap();
        assert_eq!(c.min_expiry_ms, Some(3_600_000));
        let c = parse(&[
            "print-bearer-token",
            "--provider",
            "deepseek",
            "--min-expiry",
            "500ms",
        ])
        .unwrap();
        assert_eq!(c.min_expiry_ms, Some(500));
    }

    #[test]
    fn rejects_flag_for_wrong_command() {
        assert!(parse(&["print-api-key", "--min-expiry", "30m"]).is_err());
        assert!(parse(&["check", "--min-expiry", "30m"]).is_err());
        assert!(parse(&["print-api-key", "--json"]).is_err());
        assert!(parse(&["print-bearer-token", "--credentials"]).is_err());
    }

    #[test]
    fn rejects_unknown_command_and_option() {
        let e = parse(&["bogus"]).unwrap_err();
        assert!(e.contains("Unknown auth command \"bogus\""));
        let e = parse(&["print-api-key", "--provider", "deepseek", "--wat"]).unwrap_err();
        assert!(e.contains("Unknown option --wat"));
        assert!(e.contains("auth print-api-key"));
    }

    #[test]
    fn rejects_invalid_duration() {
        assert!(
            parse(&[
                "print-bearer-token",
                "--provider",
                "x",
                "--min-expiry",
                "abc"
            ])
            .is_err()
        );
        assert!(
            parse(&[
                "print-bearer-token",
                "--provider",
                "x",
                "--min-expiry",
                "10x"
            ])
            .is_err()
        );
    }

    #[test]
    fn rejects_missing_provider_model() {
        // check 需要 --provider/--model
        let e = validate_provider_model(AuthCommandKind::Check, None, None).unwrap_err();
        assert!(e.contains("Auth checks require"));
        // print 需要 --provider/--model
        let e = validate_provider_model(AuthCommandKind::ApiKey, None, None).unwrap_err();
        assert!(e.contains("Credential printing requires"));
    }

    #[test]
    fn print_api_key_roundtrip() {
        let _g = AUTH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        test_dir();
        reset_auth();
        let c = parse(&["print-api-key", "--provider", "deepseek"]).unwrap();
        assert_eq!(run_print(&c), 1); // 未配置 → 失败
        write_auth_key("deepseek", "sk-auth-cmd").unwrap();
        assert_eq!(run_print(&c), 0);
        write_auth_key("deepseek", "sk-auth-cmd-2").unwrap();
        assert_eq!(
            resolve_credential_for_print(AuthCommandKind::ApiKey, Some("deepseek"), None).unwrap(),
            "sk-auth-cmd-2"
        );
        remove_auth("deepseek").ok();
    }

    #[test]
    fn print_api_key_unknown_provider() {
        let e =
            resolve_credential_for_print(AuthCommandKind::ApiKey, Some("nope"), None).unwrap_err();
        assert!(e.contains("Unknown provider \"nope\""));
        assert!(e.contains("--list-models"));
    }

    #[test]
    fn print_api_key_resolves_model() {
        let _g = AUTH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        test_dir();
        reset_auth();
        write_auth_key("deepseek", "sk-model-key").unwrap();
        // --model 带 provider 前缀
        let v = resolve_credential_for_print(
            AuthCommandKind::ApiKey,
            None,
            Some("deepseek/deepseek-flash"),
        )
        .unwrap();
        assert_eq!(v, "sk-model-key");
        // 无 --provider 时遍历有凭据的 provider
        let v = resolve_credential_for_print(AuthCommandKind::ApiKey, None, None).unwrap();
        assert_eq!(v, "sk-model-key");
        remove_auth("deepseek").ok();
    }

    #[test]
    fn print_bearer_token_requires_oauth() {
        let _g = AUTH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        test_dir();
        reset_auth();
        write_auth_key("deepseek", "sk-bearer").unwrap();
        // api_key 凭据不是 OAuth → 明确报错
        let e = resolve_credential_for_print(AuthCommandKind::BearerToken, Some("deepseek"), None)
            .unwrap_err();
        assert!(e.contains("is not configured with an OAuth bearer token"));
        let e = resolve_credential_for_print(AuthCommandKind::BearerToken, None, None).unwrap_err();
        assert!(e.contains("is not configured with an OAuth bearer token"));
        remove_auth("deepseek").ok();
    }

    #[test]
    fn check_ready_and_not_ready() {
        let _g = AUTH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        test_dir();
        reset_auth();
        assert_eq!(check_provider_auth("deepseek", false).status, "not_ready");
        assert_eq!(
            check_provider_auth("deepseek", false).reason,
            Some("credentials_not_configured")
        );
        write_auth_key("deepseek", "sk-check").unwrap();
        let r = check_provider_auth("deepseek", false);
        assert_eq!(r.status, "ready");
        assert_eq!(r.auth_type, Some("api_key"));
        assert_eq!(r.credential.as_deref(), Some("sk-check"));
        remove_auth("deepseek").ok();
    }

    #[test]
    fn check_unknown_provider() {
        let r = check_provider_auth("nope", false);
        assert_eq!(r.status, "not_ready");
        assert_eq!(r.reason, Some("provider_not_found"));
    }

    #[test]
    fn check_oauth_not_expired_ready_without_network() {
        // 非 no_refresh：未过期 oauth 不发网络直接 ready（credential 带有效 access token）
        let _g = AUTH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        test_dir();
        reset_auth();
        let cred = simple_credential("acc-1", "ref-1", now_ms() + 3_600_000);
        write_oauth_credential("anthropic", &cred).unwrap();
        let r = check_provider_auth("anthropic", false);
        assert_eq!(r.status, "ready");
        assert_eq!(r.auth_type, Some("oauth"));
        assert_eq!(r.credential.as_deref(), Some("acc-1"));
        remove_auth("anthropic").ok();
    }

    #[test]
    fn check_oauth_no_refresh_skips_refresh() {
        // 过期 + --no-refresh：按已有 token 直接 ready（不触发刷新）
        let _g = AUTH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        test_dir();
        reset_auth();
        let cred = simple_credential("acc-old", "ref-1", now_ms() - 1);
        write_oauth_credential("anthropic", &cred).unwrap();
        let r = check_provider_auth("anthropic", true);
        assert_eq!(r.status, "ready");
        assert_eq!(r.credential.as_deref(), Some("acc-old"));
        remove_auth("anthropic").ok();
    }

    #[test]
    fn check_oauth_permanent_and_unrefreshable_ready() {
        // 永久凭据 / 过期但无 refresh：ensure_oauth_valid 不联网返回 access → ready
        let _g = AUTH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        test_dir();
        reset_auth();
        let perm = OAuthCredential {
            access: "perm".into(),
            refresh: String::new(),
            expires: u64::MAX,
            ..Default::default()
        };
        write_oauth_credential("anthropic", &perm).unwrap();
        let r = check_provider_auth("anthropic", false);
        assert_eq!(r.status, "ready");
        assert_eq!(r.credential.as_deref(), Some("perm"));
        remove_auth("anthropic").ok();
        let exp = simple_credential("acc-exp", "", now_ms() - 1);
        write_oauth_credential("anthropic", &exp).unwrap();
        let r = check_provider_auth("anthropic", false);
        assert_eq!(r.status, "ready");
        assert_eq!(r.credential.as_deref(), Some("acc-exp"));
        remove_auth("anthropic").ok();
    }

    #[test]
    fn check_oauth_unknown_provider_not_ready() {
        // 已知 provider 但无任何凭据 → not_ready，不触发网络
        let _g = AUTH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        test_dir();
        reset_auth();
        let r = check_provider_auth("anthropic", false);
        assert_eq!(r.status, "not_ready");
        assert_eq!(r.reason, Some("credentials_not_configured"));
        remove_auth("anthropic").ok();
    }

    #[test]
    fn run_check_exit_codes() {
        let _g = AUTH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        test_dir();
        reset_auth();
        // not_ready → 1
        let c = parse(&["check", "--provider", "deepseek"]).unwrap();
        assert_eq!(run_check(&c), 1);
        // 无 --provider/--model → 2
        let c = parse(&["check"]).unwrap();
        assert_eq!(run_check(&c), 2);
        write_auth_key("deepseek", "sk-exit").unwrap();
        // ready → 0（无提供者时仍无法解析 provider）
        assert_eq!(run_check(&c), 2);
        let c2 = parse(&["check", "--provider", "deepseek"]).unwrap();
        assert_eq!(run_check(&c2), 0);
        // --credentials + 就绪 → 0（输出凭据）
        let c3 = parse(&["check", "--provider", "deepseek", "--credentials"]).unwrap();
        assert_eq!(run_check(&c3), 0);
        remove_auth("deepseek").ok();
    }

    #[test]
    fn parse_duration_units() {
        assert_eq!(parse_duration_ms("30m"), Some(1_800_000));
        assert_eq!(parse_duration_ms("2h"), Some(7_200_000));
        assert_eq!(parse_duration_ms("250ms"), Some(250));
        assert_eq!(parse_duration_ms("10s"), Some(10_000));
        assert_eq!(parse_duration_ms("30M"), Some(1_800_000));
        assert_eq!(parse_duration_ms(""), None);
        assert_eq!(parse_duration_ms("12"), None);
    }
}
