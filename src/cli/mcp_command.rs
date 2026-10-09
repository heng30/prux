//! `mcp` 子命令：MCP 服务器配置、OAuth 登录与连通性诊断
//!
//! 用法：
//! ```text
//! prux mcp list [--json]
//! prux mcp get <name> [--json]
//! prux mcp add    <name> [--scope user|project] [--url URL | -- <command> [args...]]
//!                        [--env K=V]... [--header K=V]... [--cwd DIR] [--auth oauth|bearer|none]
//!                        [--bearer-token TOKEN] [--bearer-token-env VAR]
//!                        [--oauth-scope S] [--client-id ID] [--client-secret SECRET]
//!                        [--client-credentials] [--redirect-uri URI] [--timeout-ms MS]
//!
//! prux mcp update <name>  (同 add 的 flag)
//! prux mcp remove <name> [--scope user|project]
//! prux mcp enable|disable <name> [--scope user|project]
//! prux mcp test <name>
//! prux mcp login <name> [--no-browser]
//! prux mcp logout <name>
//! ```
//!
//! 作用域：`--scope user` = `agent_dir()/extensions/mcp/mcp.json`，`--scope project` = `<cwd>/.mcp.json`。

use crate::{
    APP_NAME,
    core::{project_trust::is_project_trusted, settings_manager::agent_dir},
    extensions::mcp::{
        callback_listener,
        config::{
            self, AuthMode, AuthSetting, ClientRegistration, ConfigScope, LoadedConfig,
            OAuthConfig, OAuthGrantType, ServerEntry,
        },
        error::{McpError, bail},
        manager, oauth, receive_callback,
    },
    utils::paths::cwd,
};
use serde::Serialize;
use std::{io::Write as _, time::Duration};
use tokio_util::sync::CancellationToken;

/// CLI 层结果（复用 mcp 扩展的错误类型）。
type McpResult<T> = std::result::Result<T, McpError>;

/// 用法帮助。
pub fn print_mcp_command_help() {
    println!(
        "Usage:\n  \
         {APP_NAME} mcp list [--json]\n  \
         {APP_NAME} mcp get <name> [--json]\n  \
         {APP_NAME} mcp add <name> [--scope user|project] [--url URL | -- <command> [args...]] [--env K=V]... [--header K=V]... [--cwd DIR] [--auth oauth|bearer|none] [--auth-provider NAME] [--bearer-token TOKEN] [--bearer-token-env VAR] [--oauth-scope S] [--client-id ID] [--client-secret SECRET] [--client-credentials] [--client-registration dynamic|cimd] [--client-metadata-url URL] [--auth-server-metadata-url URL] [--redirect-uri URI] [--timeout-ms MS] [--exposure MODE] [--description TEXT]\n  \
         {APP_NAME} mcp update <name> [same flags as add]\n  \
         {APP_NAME} mcp remove <name> [--scope user|project]\n  \
         {APP_NAME} mcp enable|disable <name> [--scope user|project]\n  \
         {APP_NAME} mcp test <name>\n  \
         {APP_NAME} mcp login <name> [--no-browser]\n  \
         {APP_NAME} mcp logout <name>\n\n\
         Auth: --auth-provider NAME uses that provider's /login token as bearer.\n\
         Scope: --scope user (agent_dir()/extensions/mcp/mcp.json) or --scope project (<cwd>/.mcp.json).\n\
         Exposure: --exposure direct (default) | codemode | codemode-deferred | deferred | hidden."
    );
}

/// 解析出的 `add`/`update` 服务器参数（未套用到已有条目）。
#[derive(Debug, Default, Clone)]
struct ServerPatch {
    /// `--url`：Streamable HTTP 端点；设置后清空 stdio 相关字段。
    url: Option<String>,
    /// `--` 之后的 stdio 命令：首个元素是命令，其余是参数（与 `--url` 互斥）。
    stdio: Vec<String>,
    /// `--env K=V`：追加到子进程的环境变量，可重复。
    env: Vec<String>,
    /// `--header K=V`：HTTP 请求头，可重复。
    headers: Vec<String>,
    /// `--cwd DIR`：stdio 子进程的工作目录。
    cwd: Option<String>,
    /// `--auth`：`oauth` / `bearer` / `none` 的原始字符串，套用时才转成 [`AuthSetting`]。
    auth: Option<String>,
    /// `--bearer-token TOKEN`：bearer token 字面值（与 `bearer_token_env` 互斥）。
    bearer_token: Option<String>,
    /// `--bearer-token-env VAR`：存放 token 的环境变量名。
    bearer_token_env: Option<String>,
    /// `--oauth-scope S`：请求的 OAuth scope。
    oauth_scope: Option<String>,
    /// `--client-id ID`：预注册的 OAuth 客户端 ID。
    client_id: Option<String>,
    /// `--client-secret SECRET`：OAuth 客户端密钥。
    client_secret: Option<String>,
    /// `--client-credentials`：改用客户端凭据流程（无需浏览器）。
    client_credentials: bool,
    /// `--redirect-uri URI`：OAuth 回调地址。
    redirect_uri: Option<String>,
    /// `--timeout-ms MS`：单次 RPC 超时（毫秒），必须大于 0。
    timeout_ms: Option<u64>,
    /// `--exposure MODE`：服务器工具的默认暴露方式
    /// （`direct` / `codemode` / `codemode-deferred` / `deferred` / `hidden`）。
    exposure: Option<String>,
    /// `--description TEXT`：人类可读的说明（进 `mcp` 工具描述与 `/mcp` 列表）。
    description: Option<String>,
    /// `--auth-provider NAME`：用该 provider 的登录 token 作 bearer。
    auth_provider: Option<String>,
    /// `--client-registration MODE`：`dynamic`（默认）/ `cimd`。
    client_registration: Option<String>,
    /// `--client-metadata-url URL`：CIMD 的 metadata 文档地址。
    client_metadata_url: Option<String>,
    /// `--auth-server-metadata-url URL`：跳过发现流程，直接用该 metadata 文档。
    auth_server_metadata_url: Option<String>,
}

/// `parse` 解析出的子命令及其参数（每个变体对应一个 `prux mcp <sub>`）。
#[derive(Debug)]
enum Cli {
    /// `list`：列出全部已配置服务器。
    List {
        /// `--json`：输出 JSON 而非表格。
        json: bool,
    },
    /// `get`：查看单个服务器（含脱敏后的配置与来源）。
    Get {
        /// 服务器名。
        name: String,
        /// `--json`：输出 JSON。
        json: bool,
    },
    /// `add`：新增服务器。
    Add {
        /// 服务器名。
        name: String,
        /// `--scope`：写入用户配置还是项目 `.mcp.json`。
        scope: ConfigScope,
        /// 待套用的命令行参数。
        server: ServerPatch,
    },
    /// `update`：在已有条目上套用同样的参数。
    Update {
        /// 服务器名。
        name: String,
        /// `--scope`：写入用户配置还是项目 `.mcp.json`。
        scope: ConfigScope,
        /// 待套用的命令行参数。
        server: ServerPatch,
    },
    /// `remove`：从指定作用域删除服务器。
    Remove {
        /// 服务器名。
        name: String,
        /// `--scope`：要删除的位置。
        scope: ConfigScope,
    },
    /// `enable`：清除禁用标记。
    Enable {
        /// 服务器名。
        name: String,
        /// `--scope`：要修改的位置。
        scope: ConfigScope,
    },
    /// `disable`：置上禁用标记（不再连接、不暴露工具）。
    Disable {
        /// 服务器名。
        name: String,
        /// `--scope`：要修改的位置。
        scope: ConfigScope,
    },
    /// `test`：连接服务器并列出工具。
    Test {
        /// 服务器名。
        name: String,
    },
    /// `login`：执行 OAuth 登录并保存凭据。
    Login {
        /// 服务器名。
        name: String,
        /// `--no-browser`：不打开浏览器，直接要求粘贴回调 URL。
        no_browser: bool,
    },
    /// `logout`：删除已保存的 OAuth 凭据。
    Logout {
        /// 服务器名。
        name: String,
    },
}

/// 确认服务器在合并视图里存在：`enable`/`disable` 只写最小覆盖条目，
/// 没有对应服务器时写下去就是一条悬空覆盖（加载时会以「缺 command/url」报错）。
fn require_configured(loaded: &LoadedConfig, name: &str) -> McpResult<()> {
    if loaded.config.mcp_servers.contains_key(name) {
        return Ok(());
    }
    Err(McpError::Message(format!(
        "MCP server `{name}` is not configured"
    )))
}

/// 运行 `prux mcp ...`。返回进程退出码。
pub async fn run_mcp_command(args: &[String]) -> i32 {
    if args.is_empty()
        || args.first().map(String::as_str) == Some("help")
        || args.iter().any(|a| a == "--help" || a == "-h")
    {
        print_mcp_command_help();
        return 0;
    }

    let cli = match parse(args) {
        Ok(cli) => cli,
        Err(error) => {
            eprintln!("Error: {error}");
            eprintln!();
            print_mcp_command_help();
            return 1;
        }
    };

    match run(cli).await {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("Error: {error}");
            1
        }
    }
}

/// 当前工作目录是否已被用户标记为受信任项目。
fn trusted() -> bool {
    is_project_trusted(&cwd(), &agent_dir())
}

/// 加载合并后的 MCP 配置（按当前目录的信任状态决定是否读入项目级 `.mcp.json`）。
fn load() -> McpResult<LoadedConfig> {
    config::load(&cwd(), trusted())
}

/// 分发已解析的 `mcp` 子命令；成功时返回 `Ok(())`，配置读写或网络操作失败时返回错误。
async fn run(cli: Cli) -> McpResult<()> {
    match cli {
        Cli::List { json } => {
            let loaded = load()?;
            if json {
                let servers = loaded
                    .config
                    .mcp_servers
                    .iter()
                    .map(|(name, server)| {
                        serde_json::json!({
                            "name": name,
                            "server": server.redacted(),
                            "sources": loaded.source_labels(name),
                        })
                    })
                    .collect::<Vec<_>>();
                print_json(&servers)?;
            } else if loaded.config.mcp_servers.is_empty() {
                println!("No MCP servers are configured.");
            } else {
                for (name, server) in &loaded.config.mcp_servers {
                    let description = match server.description.as_deref().map(str::trim) {
                        Some(description) if !description.is_empty() => {
                            format!("\t{description}")
                        }
                        _ => String::new(),
                    };

                    println!(
                        "{name}\t{}\t{}{description}",
                        state_label(server),
                        transport_label(server)
                    );
                }
            }
        }
        Cli::Get { name, json } => {
            let loaded = load()?;
            let server = loaded.config.mcp_servers.get(&name).ok_or_else(|| {
                McpError::Message(format!("MCP server `{name}` is not configured"))
            })?;
            let value = serde_json::json!({
                "name": name,
                "server": server.redacted(),
                "sources": loaded.source_labels(&name),
            });
            if json {
                print_json(&value)?;
            } else {
                println!("{}", serde_json::to_string_pretty(&value)?);
            }
        }
        Cli::Add {
            name,
            scope,
            server,
        } => {
            let loaded = load()?;
            let entry = apply_patch(ServerEntry::default(), &server, true)?;
            config::add_server(&loaded.paths, scope, &name, entry)?;
            println!("Added MCP server `{name}` to {}.", scope_label(scope));
        }
        Cli::Update {
            name,
            scope,
            server,
        } => {
            let loaded = load()?;
            let base = loaded
                .config
                .mcp_servers
                .get(&name)
                .cloned()
                .ok_or_else(|| {
                    McpError::Message(format!("MCP server `{name}` is not configured"))
                })?;
            let entry = apply_patch(base, &server, false)?;
            config::put_server(&loaded.paths, scope, &name, entry)?;
            println!("Updated MCP server `{name}` in {}.", scope_label(scope));
        }
        Cli::Remove { name, scope } => {
            let loaded = load()?;
            if config::remove_server(&loaded.paths, scope, &name)? {
                println!("Removed MCP server `{name}` from {}.", scope_label(scope));
            } else {
                println!(
                    "MCP server `{name}` is not present in {}.",
                    scope_label(scope)
                );
            }
        }
        Cli::Enable { name, scope } => {
            let loaded = load()?;
            require_configured(&loaded, &name)?;
            config::set_disabled(&loaded.paths, scope, &name, false)?;
            println!("Enabled MCP server `{name}` in {}.", scope_label(scope));
        }
        Cli::Disable { name, scope } => {
            let loaded = load()?;
            require_configured(&loaded, &name)?;
            config::set_disabled(&loaded.paths, scope, &name, true)?;
            println!("Disabled MCP server `{name}` in {}.", scope_label(scope));
        }
        Cli::Test { name } => {
            let loaded = load()?;
            let manager = manager::McpManager::new(loaded)?;
            let cancel = tokio_util::sync::CancellationToken::new();
            let tools = manager.list_tools(Some(&name), &cancel).await?;
            println!(
                "Connected to MCP server `{name}`. Found {} tools.",
                tools.len()
            );
            manager.disconnect_all().await;
        }
        Cli::Login { name, no_browser } => {
            login(&name, no_browser).await?;
            println!("Saved OAuth credentials for MCP server `{name}`.");
        }
        Cli::Logout { name } => {
            let loaded = load()?;
            let server = http_server(&loaded, &name)?;
            let url = server
                .url
                .as_deref()
                .ok_or_else(|| McpError::Message("MCP URL is missing".to_string()))?;
            if oauth::logout(&name, url).await? {
                println!("Removed OAuth credentials for MCP server `{name}`.");
            } else {
                println!("No OAuth credentials are saved for MCP server `{name}`.");
            }
        }
    }
    Ok(())
}

// `Enable`/`Disable` 需要区分，单独处理以免用 unreachable 占位。
/// 服务器启用状态的可读标签（`enabled`/`disabled`）。
fn state_label(server: &ServerEntry) -> &'static str {
    if server.disabled {
        "disabled"
    } else {
        "enabled"
    }
}

/// 传输方式的可读描述：有 `url` 时是 `HTTP <url>`，否则是 `stdio <command>`。
fn transport_label(server: &ServerEntry) -> String {
    server
        .url
        .as_deref()
        .map(|url| format!("HTTP {url}"))
        .unwrap_or_else(|| format!("stdio {}", server.command.as_deref().unwrap_or("")))
}

/// 配置作用域的可读名称（用户级配置或项目级 `.mcp.json`）。
fn scope_label(scope: ConfigScope) -> &'static str {
    match scope {
        ConfigScope::User => "the user configuration",
        ConfigScope::Project => ".mcp.json",
    }
}

/// 取出名为 `name` 的服务器，且要求它是 HTTP 类型。
///
/// 未配置或为 stdio 服务器时返回错误（后者因 OAuth 仅适用于 HTTP）。
fn http_server<'a>(loaded: &'a LoadedConfig, name: &str) -> McpResult<&'a ServerEntry> {
    let server = loaded
        .config
        .mcp_servers
        .get(name)
        .ok_or_else(|| McpError::Message(format!("MCP server `{name}` is not configured")))?;

    if server.url.is_none() {
        bail!("MCP server `{name}` is a stdio server and does not use OAuth")
    }

    Ok(server)
}

/// 把命令行给出的 `patch` 合并到 `server` 上并返回结果。
///
/// `creating` 为真表示新建，此时未提供 `--url` 或 stdio 命令会报错；
/// 提供 URL 会清空 stdio 相关字段，反之亦然。参数值非法（如 `--auth` 取值、
/// 非正数超时）时返回错误。
fn apply_patch(
    mut server: ServerEntry,
    patch: &ServerPatch,
    creating: bool,
) -> McpResult<ServerEntry> {
    if let Some(url) = &patch.url {
        server.url = Some(url.clone());
        server.command = None;
        server.args.clear();
    } else if !patch.stdio.is_empty() {
        server.command = Some(patch.stdio[0].clone());
        server.args = patch.stdio[1..].to_vec();
        server.url = None;
        server.headers.clear();
        server.auth = None;
        server.oauth = None;
        server.bearer_token = None;
        server.bearer_token_env = None;
    } else if creating {
        bail!("MCP server needs --url URL or a stdio command after `--`")
    }

    if !patch.env.is_empty() {
        server.env = config::parse_pairs(&patch.env, "MCP environment value")?;
    }

    if !patch.headers.is_empty() {
        server.headers = config::parse_pairs(&patch.headers, "MCP header")?;
    }

    if let Some(cwd) = &patch.cwd {
        server.cwd = Some(cwd.clone());
    }

    if let Some(auth) = &patch.auth {
        server.auth = Some(match auth.as_str() {
            "oauth" => AuthSetting::Mode(AuthMode::OAuth),
            "bearer" => AuthSetting::Mode(AuthMode::Bearer),
            "none" => AuthSetting::Disabled(false),
            _ => bail!("MCP --auth must be oauth, bearer, or none"),
        });
    }

    if let Some(token) = &patch.bearer_token {
        server.bearer_token = Some(token.clone());
        server.bearer_token_env = None;
    }

    if let Some(name) = &patch.bearer_token_env {
        server.bearer_token_env = Some(name.clone());
        server.bearer_token = None;
    }

    if let Some(description) = &patch.description {
        server.description = Some(description.clone());
    }

    if let Some(provider) = &patch.auth_provider {
        server.auth = Some(AuthSetting::Provider {
            provider: provider.clone(),
        });
    }

    if let Some(exposure) = &patch.exposure {
        let Some(exposure) = config::McpExposure::parse(exposure) else {
            bail!("MCP --exposure must be direct, codemode, codemode-deferred, deferred, or hidden")
        };
        server.exposure = Some(exposure);
    }

    let oauth_changed = patch.oauth_scope.is_some()
        || patch.client_id.is_some()
        || patch.client_secret.is_some()
        || patch.client_credentials
        || patch.redirect_uri.is_some()
        || patch.client_registration.is_some()
        || patch.client_metadata_url.is_some()
        || patch.auth_server_metadata_url.is_some();

    if oauth_changed || matches!(server.auth, Some(AuthSetting::Mode(AuthMode::OAuth))) {
        let oauth = server.oauth.get_or_insert_with(OAuthConfig::default);
        if let Some(scope) = &patch.oauth_scope {
            oauth.scope = Some(scope.clone());
        }
        if let Some(client_id) = &patch.client_id {
            oauth.client_id = Some(client_id.clone());
        }
        if let Some(client_secret) = &patch.client_secret {
            oauth.client_secret = Some(client_secret.clone());
        }
        if patch.client_credentials {
            oauth.grant_type = OAuthGrantType::ClientCredentials;
        }
        if let Some(registration) = &patch.client_registration {
            let Some(registration) = ClientRegistration::parse(registration) else {
                bail!(
                    "MCP --client-registration must be {} or {}",
                    ClientRegistration::Dynamic.as_str(),
                    ClientRegistration::Cimd.as_str()
                )
            };
            oauth.client_registration = registration;
        }
        if let Some(url) = &patch.client_metadata_url {
            oauth.client_metadata_url = Some(url.clone());
        }
        if let Some(url) = &patch.auth_server_metadata_url {
            oauth.auth_server_metadata_url = Some(url.clone());
        }
        if let Some(redirect_uri) = &patch.redirect_uri {
            oauth.redirect_uri = Some(redirect_uri.clone());
        }
    }

    if let Some(timeout) = patch.timeout_ms {
        if timeout == 0 {
            bail!("MCP --timeout-ms must be more than zero")
        }
        server.request_timeout_ms = Some(timeout);
    }

    Ok(server)
}

/// 对 HTTP 服务器执行 OAuth 登录并保存凭据。
///
/// 客户端凭据模式直接换 token；否则先探测授权端点再引导用户授权。
/// `no_browser` 为真或无法打开本地回调监听时，改为让用户手动粘贴回调 URL。
async fn login(name: &str, no_browser: bool) -> McpResult<()> {
    let loaded = load()?;
    let server = http_server(&loaded, name)?;
    let url = server
        .url
        .as_deref()
        .ok_or_else(|| McpError::Message("MCP URL is missing".to_string()))?;
    let oauth_config = server.oauth.clone().unwrap_or_default();
    if oauth_config.grant_type == OAuthGrantType::ClientCredentials {
        return oauth::login_client_credentials(name, url, &oauth_config).await;
    }

    let challenge = tokio::time::timeout(
        Duration::from_secs(15),
        manager::probe_oauth_challenge(server),
    )
    .await
    .map_err(|_| McpError::Message("MCP OAuth discovery timed out".to_string()))??;

    let pending = oauth::begin_login(name, url, &oauth_config, challenge.as_deref()).await?;
    println!("Open this URL to sign in:\n{}", pending.authorization_url);

    if no_browser {
        let callback = prompt_line("Paste the full redirect URL: ")?;
        return oauth::finish_login(pending, &callback).await;
    }

    match callback_listener(&pending.redirect_uri).await {
        Ok(Some(listener)) => {
            if webbrowser::open(&pending.authorization_url).is_err() {
                eprintln!("The browser did not open. Open the URL manually.");
            }

            let callback =
                receive_callback(listener, &pending.redirect_uri, &CancellationToken::new())
                    .await
                    .map_err(McpError::Message)?;
            oauth::finish_login(pending, &callback).await
        }
        _ => {
            if webbrowser::open(&pending.authorization_url).is_err() {
                eprintln!("The browser did not open. Open the URL manually.");
            }

            let callback = prompt_line("Paste the full redirect URL: ")?;
            oauth::finish_login(pending, &callback).await
        }
    }
}

/// 打印提示并读取标准输入一行，返回去掉首尾空白的内容；I/O 失败时返回错误。
fn prompt_line(prompt: &str) -> McpResult<String> {
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut value = String::new();
    std::io::stdin().read_line(&mut value)?;
    Ok(value.trim().to_string())
}

/// 以缩进 JSON 打印任意可序列化值。
fn print_json(value: &impl Serialize) -> McpResult<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

/// 参数解析
fn parse(args: &[String]) -> Result<Cli, String> {
    let mut it = args.iter();
    let sub = it.next().ok_or_else(|| "missing subcommand".to_string())?;

    match sub.as_str() {
        "list" => {
            let mut json = false;
            for arg in it {
                match arg.as_str() {
                    "--json" => json = true,
                    other => return Err(format!("unknown flag for `list`: {other}")),
                }
            }
            Ok(Cli::List { json })
        }
        "get" => {
            let mut name = None;
            let mut json = false;

            for arg in it {
                match arg.as_str() {
                    "--json" => json = true,
                    other if other.starts_with('-') => {
                        return Err(format!("unknown flag for `get`: {other}"));
                    }
                    other => set_once(&mut name, other, "server name")?,
                }
            }

            Ok(Cli::Get {
                name: name.ok_or_else(|| "get needs a server name".to_string())?,
                json,
            })
        }
        "add" | "update" => {
            let mut name = None;
            let mut scope = ConfigScope::User;
            let mut patch = ServerPatch::default();

            parse_server_args(it, &mut scope, &mut patch, &mut name)?;

            let name = name.ok_or_else(|| format!("{sub} needs a server name"))?;

            Ok(if sub == "add" {
                Cli::Add {
                    name,
                    scope,
                    server: patch,
                }
            } else {
                Cli::Update {
                    name,
                    scope,
                    server: patch,
                }
            })
        }
        "remove" | "enable" | "disable" => {
            let mut name = None;
            let mut scope = ConfigScope::User;
            while let Some(arg) = it.next() {
                let (flag, inline) = match arg.split_once('=') {
                    Some((flag, value)) => (flag, Some(value.to_string())),
                    None => (arg.as_str(), None),
                };

                match flag {
                    "--scope" => {
                        let value = match inline {
                            Some(value) => value,
                            None => it
                                .next()
                                .cloned()
                                .ok_or_else(|| "--scope needs a value".to_string())?,
                        };
                        scope = parse_scope_str(&value)?;
                    }
                    other if other.starts_with('-') => {
                        return Err(format!("unknown flag for `{sub}`: {other}"));
                    }
                    other => set_once(&mut name, other, "server name")?,
                }
            }

            let name = name.ok_or_else(|| format!("{sub} needs a server name"))?;

            Ok(match sub.as_str() {
                "remove" => Cli::Remove { name, scope },
                "enable" => Cli::Enable { name, scope },
                _ => Cli::Disable { name, scope },
            })
        }
        "test" => {
            let name = it
                .next()
                .ok_or_else(|| "test needs a server name".to_string())?
                .clone();
            Ok(Cli::Test { name })
        }
        "login" => {
            let mut name = None;
            let mut no_browser = false;

            for arg in it {
                match arg.as_str() {
                    "--no-browser" => no_browser = true,
                    other if other.starts_with('-') => {
                        return Err(format!("unknown flag for `login`: {other}"));
                    }
                    other => set_once(&mut name, other, "server name")?,
                }
            }
            Ok(Cli::Login {
                name: name.ok_or_else(|| "login needs a server name".to_string())?,
                no_browser,
            })
        }
        "logout" => {
            let name = it
                .next()
                .ok_or_else(|| "logout needs a server name".to_string())?
                .clone();
            Ok(Cli::Logout { name })
        }
        other => Err(format!("unknown mcp subcommand `{other}`")),
    }
}

/// 向 `slot` 写入一次值；若此前已赋值则报「`label` 重复指定」错误。
fn set_once(slot: &mut Option<String>, value: &str, label: &str) -> Result<(), String> {
    if slot.is_some() {
        return Err(format!("{label} specified more than once"));
    }

    *slot = Some(value.to_string());
    Ok(())
}

/// 解析 `--scope` 的取值（`user`/`project`），无法识别时返回错误信息。
fn parse_scope_str(value: &str) -> Result<ConfigScope, String> {
    match value {
        "user" => Ok(ConfigScope::User),
        "project" => Ok(ConfigScope::Project),
        other => Err(format!("--scope must be user or project, got `{other}`")),
    }
}

/// 解析 `add`/`update` 的 flag 与 `--` 之后的 stdio 命令。
fn parse_server_args<'a>(
    mut it: impl Iterator<Item = &'a String>,
    scope: &mut ConfigScope,
    patch: &mut ServerPatch,
    name: &mut Option<String>,
) -> Result<(), String> {
    while let Some(arg) = it.next() {
        if arg == "--" {
            patch.stdio = it.cloned().collect();
            break;
        }

        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) => (flag, Some(value.to_string())),
            None => (arg.as_str(), None),
        };

        // 取值：优先 inline（--k=v），否则消费下一个。
        let mut value = |flag: &str| -> Result<String, String> {
            if let Some(value) = inline.clone() {
                return Ok(value);
            }

            it.next()
                .cloned()
                .ok_or_else(|| format!("{flag} needs a value"))
        };

        match flag {
            "--scope" => *scope = parse_scope_str(&value("--scope")?)?,
            "--url" => patch.url = Some(value("--url")?),
            "--env" => patch.env.push(value("--env")?),
            "--header" => patch.headers.push(value("--header")?),
            "--cwd" => patch.cwd = Some(value("--cwd")?),
            "--auth" => patch.auth = Some(value("--auth")?),
            "--bearer-token" => patch.bearer_token = Some(value("--bearer-token")?),
            "--bearer-token-env" => patch.bearer_token_env = Some(value("--bearer-token-env")?),
            "--oauth-scope" => patch.oauth_scope = Some(value("--oauth-scope")?),
            "--client-id" => patch.client_id = Some(value("--client-id")?),
            "--client-secret" => patch.client_secret = Some(value("--client-secret")?),
            "--redirect-uri" => patch.redirect_uri = Some(value("--redirect-uri")?),
            "--timeout-ms" => {
                let raw = value("--timeout-ms")?;
                patch.timeout_ms = Some(
                    raw.parse()
                        .map_err(|_| format!("--timeout-ms must be a number, got `{raw}`"))?,
                );
            }
            "--exposure" => patch.exposure = Some(value("--exposure")?),
            "--description" => patch.description = Some(value("--description")?),
            "--auth-provider" => patch.auth_provider = Some(value("--auth-provider")?),
            "--client-registration" => {
                patch.client_registration = Some(value("--client-registration")?)
            }
            "--client-metadata-url" => {
                patch.client_metadata_url = Some(value("--client-metadata-url")?)
            }
            "--auth-server-metadata-url" => {
                patch.auth_server_metadata_url = Some(value("--auth-server-metadata-url")?)
            }
            "--client-credentials" => patch.client_credentials = true,
            other if other.starts_with('-') => {
                return Err(format!("unknown flag for add/update: {other}"));
            }
            other => set_once(name, other, "server name")?,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_stdio_add_with_env() {
        let cli = parse(&args(&[
            "add",
            "demo",
            "--scope",
            "project",
            "--env",
            "TOKEN=value",
            "--",
            "demo",
            "--stdio",
        ]))
        .unwrap();
        match cli {
            Cli::Add {
                name,
                scope,
                server,
            } => {
                assert_eq!(name, "demo");
                assert_eq!(scope, ConfigScope::Project);
                assert_eq!(server.stdio, ["demo", "--stdio"]);
                assert_eq!(server.env, ["TOKEN=value"]);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn parses_url_add_with_inline_flags() {
        let cli = parse(&args(&[
            "add",
            "web",
            "--url=https://example.com/mcp",
            "--auth=oauth",
            "--client-credentials",
            "--timeout-ms",
            "5000",
        ]))
        .unwrap();
        match cli {
            Cli::Add { server, .. } => {
                assert_eq!(server.url.as_deref(), Some("https://example.com/mcp"));
                assert_eq!(server.auth.as_deref(), Some("oauth"));
                assert!(server.client_credentials);
                assert_eq!(server.timeout_ms, Some(5000));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn rejects_unknown_flag() {
        assert!(parse(&args(&["list", "--bogus"])).is_err());
        assert!(parse(&args(&["get"])).is_err());
        assert!(parse(&args(&["add", "x"])).is_ok()); // 缺 transport 在 apply_patch 时报错
    }

    #[test]
    fn apply_patch_builds_server_and_validates() {
        let patch = ServerPatch {
            url: Some("https://example.com/mcp".to_string()),
            env: vec!["A=1".to_string()],
            ..Default::default()
        };
        let entry = apply_patch(ServerEntry::default(), &patch, true).unwrap();
        assert_eq!(entry.env["A"], "1");
        entry.validate("demo").unwrap();
        assert!(apply_patch(ServerEntry::default(), &ServerPatch::default(), true).is_err());
    }

    /// 2.9 / 2.10 / 2.11 / 2.12：新增 flag 写进对应字段，非法取值报错。
    #[test]
    fn new_flags_populate_server_fields() {
        let cli = parse(&args(&[
            "add",
            "demo",
            "--url=https://example.com/mcp",
            "--description",
            "Issue tracker tools",
            "--auth",
            "oauth",
            "--client-registration",
            "cimd",
            "--client-metadata-url",
            "https://example.com/client-metadata.json",
            "--auth-server-metadata-url",
            "https://example.com/.well-known/oauth-authorization-server",
        ]))
        .unwrap();
        let Cli::Add { server, .. } = cli else {
            panic!("expected add")
        };
        let entry = apply_patch(ServerEntry::default(), &server, true).unwrap();
        entry.validate("demo").unwrap();
        assert_eq!(entry.description.as_deref(), Some("Issue tracker tools"));

        let oauth = entry.oauth.as_ref().expect("oauth block");
        assert_eq!(oauth.client_registration, ClientRegistration::Cimd);
        assert_eq!(
            oauth.client_metadata_url.as_deref(),
            Some("https://example.com/client-metadata.json")
        );
        assert_eq!(
            oauth.auth_server_metadata_url.as_deref(),
            Some("https://example.com/.well-known/oauth-authorization-server")
        );

        // cimd 必须配 clientMetadataUrl，否则校验不过
        let cli = parse(&args(&[
            "add",
            "demo",
            "--url=https://example.com/mcp",
            "--auth",
            "oauth",
            "--client-registration",
            "cimd",
        ]))
        .unwrap();
        let Cli::Add { server, .. } = cli else {
            panic!("expected add")
        };
        let entry = apply_patch(ServerEntry::default(), &server, true).unwrap();
        assert!(entry.validate("demo").is_err());

        // 非法取值
        let cli = parse(&args(&[
            "add",
            "demo",
            "--url=https://example.com/mcp",
            "--client-registration",
            "bogus",
        ]))
        .unwrap();
        let Cli::Add { server, .. } = cli else {
            panic!("expected add")
        };
        assert!(apply_patch(ServerEntry::default(), &server, true).is_err());
    }

    /// 2.11：`--auth-provider` 写入 `auth: {"provider": ...}`，归入 bearer 模式。
    #[test]
    fn auth_provider_flag_sets_provider_auth() {
        let cli = parse(&args(&[
            "add",
            "demo",
            "--url=https://example.com/mcp",
            "--auth-provider",
            "deepseek",
        ]))
        .unwrap();
        let Cli::Add { server, .. } = cli else {
            panic!("expected add")
        };
        let entry = apply_patch(ServerEntry::default(), &server, true).unwrap();
        entry.validate("demo").unwrap();
        assert_eq!(entry.auth_provider(), Some("deepseek"));
        assert_eq!(entry.auth_mode(), Some(AuthMode::Bearer));
    }

    /// `--exposure` 写入服务器 exposure，取值非法时报错。
    #[test]
    fn exposure_flag_sets_server_exposure() {
        let cli = parse(&args(&[
            "add",
            "docs",
            "--url=https://example.com/mcp",
            "--exposure",
            "codemode-deferred",
        ]))
        .unwrap();
        let Cli::Add { server, .. } = cli else {
            panic!("expected add")
        };
        assert_eq!(server.exposure.as_deref(), Some("codemode-deferred"));

        let entry = apply_patch(
            ServerEntry::default(),
            &ServerPatch {
                url: Some("https://example.com/mcp".to_string()),
                exposure: server.exposure.clone(),
                ..Default::default()
            },
            true,
        )
        .unwrap();
        assert_eq!(
            entry.exposure,
            Some(crate::extensions::mcp::config::McpExposure::CodemodeDeferred)
        );

        let bad = apply_patch(
            ServerEntry::default(),
            &ServerPatch {
                url: Some("https://example.com/mcp".to_string()),
                exposure: Some("bogus".to_string()),
                ..Default::default()
            },
            true,
        );
        assert!(bad.is_err(), "非法 --exposure 应报错");
    }
}
