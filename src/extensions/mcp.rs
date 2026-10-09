//! `mcp` 扩展：Model Context Protocol 客户端
//!
//! 提供一个 `mcp` 代理工具，把已配置的 MCP 服务器的工具 / 资源 / prompt 暴露给模型；
//! 并提供 `/mcp` 斜杠命令做配置与 OAuth 登录管理。
//!
//! - **配置**：6 个位置深度合并（全局优先，后覆盖前，见 [`config`]），
//!   项目级配置仅在项目受信任时加载；
//! - **生命周期**：懒连接、空闲断开（默认 10 分钟）、每 RPC 超时 + 取消；
//! - **缓存**：tools/resources/prompts 元数据按配置指纹缓存到
//!   `agent_dir()/extensions/mcp/cache.json`，供 `search` / `describe` 免连接命中；
//! - **OAuth 2.1**：authorization code + PKCE / client credentials，
//!   凭据落 `agent_dir()/extensions/mcp/oauth.json`（见 [`oauth`]）。
//!
//! 默认不启用（启用后，会向系统提示注入工具与指引，属按需开启的工作流能力）。

pub mod config;
pub mod error;
pub mod manager;
pub mod oauth;

use crate::{
    APP_NAME,
    core::{
        extensions::{
            self, Extension, ExtensionCommand, ExtensionMode, ExtensionTool, ExtensionUiRequest,
            ForkProjectInfo, SubcommandDef, ToolAnnotations, ToolExecCtx, ToolExposure,
            UiNotifyLevel,
        },
        project_trust::is_project_trusted,
        provider::AgentMessage,
        session_manager::Session,
        settings_manager::agent_dir,
        tools::{ToolError, ToolExecutionMode, ToolResult, ToolResultAttachment},
    },
    extensions::{
        EXTENSION_FACTORIES, ExtensionFactory, PRIORITY_MCP, command_arg, util::notify_plain,
    },
    modes::interactive::{
        app::{App, MsgLevel},
        handlers::register_slash_command,
    },
    utils::{glyphs::DEF_ELLIPSIS, paths::cwd},
};
use config::{AuthMode, LoadedConfig, McpExposure, OAuthGrantType, ServerEntry};
use error::McpError;
use futures_util::future::BoxFuture;
use manager::{CachedTool, McpManager};
use rmcp::model::ContentBlock as McpContent;
use rustls::crypto::{CryptoProvider, aws_lc_rs};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    future::Future,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, Once, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::TcpListener,
};
use tokio_util::sync::CancellationToken;
use url::Url;

/// 扩展名（注册名，也是 `/extension` 面板里的名字）
///
/// `--no-mcp` 按这个名字禁用内置 MCP 扩展，因此必须公开。
pub const EXT: &str = "mcp";

/// 工具名（模型看到的聚合网关工具名）
///
/// 必须与 [`crate::core::tool_names::MCP_GATEWAY_TOOL_NAME`] 一致（由本模块的测试守住）。
pub const TOOL: &str = "mcp";

/// 斜杠命令名
const CMD: &str = "mcp";

/// 单条文本输出的字节上限（超出截断）。
const MAX_TEXT_BYTES: usize = 100_000;

/// 单台服务器的 `description` 拼进工具描述时的字符上限。
const SERVER_DESCRIPTION_CHARS: usize = 120;

/// 管理器缓存的新鲜度：同一 cwd 的配置在这么久内不重复读盘。
const MANAGER_TTL: Duration = Duration::from_secs(3);

/// 系统提示里那段服务器清单的段名（`<mcp_servers>` 标签）。
pub const MCP_SERVERS_SECTION: &str = "mcp_servers";

/// `mcp_servers` 段整体的字符上限：放不下时省略末尾若干台并补一行计数。
pub const MAX_SERVERS_SECTION_CHARS: usize = 4096;

/// `mcp_servers` 段里单台服务器摘要的字符上限
pub const MAX_SERVER_DESCRIPTION_CHARS: usize = 250;

/// `/mcp` 子命令名的竖线拼接串（仅用于错误提示）。
const SUBCOMMAND_NAMES: &str = "status|list|tools|test|login|logout|reload";

/// `/mcp` 子命令候选（顺序 = 输入框候选顺序）。
const SUBCOMMANDS: &[SubcommandDef] = &[
    SubcommandDef {
        name: "status",
        description: "Show per-server state and cached tool/resource/prompt counts",
    },
    SubcommandDef {
        name: "list",
        description: "List configured MCP servers and their sources",
    },
    SubcommandDef {
        name: "tools",
        description: "List cached tools: /mcp tools [server]",
    },
    SubcommandDef {
        name: "test",
        description: "Connect and list a server's tools: /mcp test <server>",
    },
    SubcommandDef {
        name: "login",
        description: "OAuth login: /mcp login <server> [redirect-url]",
    },
    SubcommandDef {
        name: "logout",
        description: "Remove saved OAuth credentials: /mcp logout <server>",
    },
    SubcommandDef {
        name: "reload",
        description: "Re-read MCP config files and drop cached connections",
    },
    SubcommandDef {
        name: "enable",
        description: "Enable a server: /mcp enable <server> [user|project] (default project)",
    },
    SubcommandDef {
        name: "disable",
        description: "Disable a server: /mcp disable <server> [user|project] (default project)",
    },
];

/// 自声明工厂：linkme 分布式切片。
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static MCP_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_MCP,
    make: || -> Arc<dyn Extension> { Arc::new(Mcp) },
};

/// 安装 rustls 默认 crypto provider（rmcp OAuth / reqwest 需要，幂等）。
pub fn ensure_tls_crypto_provider() {
    /// 保证 rustls 默认 crypto provider 只安装一次的幂等标记。
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        if CryptoProvider::get_default().is_none() {
            _ = aws_lc_rs::default_provider().install_default();
        }
    });
}

/// 缓存的管理器（按 cwd + 信任状态 + 读盘时刻记）。
struct CachedManager {
    /// 缓存建立时的工作目录。
    cwd: PathBuf,
    /// 缓存建立时项目是否受信任。
    trusted: bool,
    /// 已构建的管理器。
    manager: McpManager,
    /// 配置读盘时刻，用于 [`MANAGER_TTL`] 过期判定。
    loaded_at: Instant,
}

/// 全进程唯一的管理器缓存槽。
fn manager_cache() -> &'static Mutex<Option<CachedManager>> {
    /// 全进程唯一的管理器缓存槽，按 cwd、信任状态与读盘时刻复用连接。
    static C: OnceLock<Mutex<Option<CachedManager>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(None))
}

/// 使管理器缓存失效（`/mcp reload`、扩展启用状态变化时调用）。
pub fn invalidate_manager() {
    *manager_cache().lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// 取（或重建）当前 cwd 的管理器。
fn manager_for(cwd: &str) -> Result<McpManager, ToolError> {
    let cwd = PathBuf::from(if cwd.is_empty() { "." } else { cwd });
    let trusted = is_project_trusted(&cwd, &agent_dir());

    {
        let guard = manager_cache().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(cached) = guard.as_ref()
            && cached.cwd == cwd
            && cached.trusted == trusted
            && cached.loaded_at.elapsed() < MANAGER_TTL
        {
            return Ok(cached.manager.clone());
        }
    }

    let loaded = load_config_with_declarations(&cwd, trusted).map_err(ToolError)?;
    let manager = McpManager::new(loaded).map_err(|e| ToolError(e.to_string()))?;
    let mut guard = manager_cache().lock().unwrap_or_else(|e| e.into_inner());
    *guard = Some(CachedManager {
        cwd,
        trusted,
        manager: manager.clone(),
        loaded_at: Instant::now(),
    });
    Ok(manager)
}

/// `mcp` 扩展。
pub struct Mcp;

impl Extension for Mcp {
    /// 返回扩展标识常量 `mcp`（用于注册与诊断）。
    fn name(&self) -> &str {
        EXT
    }

    /// `/extension` 面板展示的描述：把已配置 MCP 服务器的工具/资源/提示代理成单个 `mcp` 工具，
    /// 附带惰性连接、元数据缓存与 OAuth 2.1 登录，并列出配置文件搜索路径。
    fn description(&self) -> &str {
        concat!(
            "Model Context Protocol client. Proxies configured MCP servers' tools, resources, and \
         prompts through one `mcp` agent tool, with lazy connections, metadata caching, and OAuth 2.1 \
         login (/mcp). Config is read from ~/.config/mcp, ~/.agents, agent_dir()/extensions/mcp/mcp.json, and \
         (trusted) project .mcp.json / ",
            ".",
            env!("CARGO_PKG_NAME"),
            "/mcp.json."
        )
    }

    /// 声明本扩展移植自 `kiss-mcp` 插件（含版本与仓库地址），供 `/extension` 详情面板展示来源。
    fn fork_project(&self) -> Option<ForkProjectInfo> {
        Some(ForkProjectInfo {
            plugin_name: "kiss-mcp".to_string(),
            plugin_version: "0.0.18".to_string(),
            url: "https://github.com/racetozero/kiss".to_string(),
        })
    }

    /// 声明仅在 Dev 与 Creator 模式下可用。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Dev, ExtensionMode::Creator]
    }

    /// 默认禁用：需在 `/extension` 面板手动开启后才注入工具与命令。
    fn default_enabled(&self) -> bool {
        false
    }

    /// 仅当当前 cwd 至少配置了一个已启用 MCP 服务器时返回 `mcp` 工具，否则返回空列表。
    fn tools(&self) -> Vec<ExtensionTool> {
        match enabled_servers() {
            Some(servers) => vec![mcp_tool(
                aggregate_exposure(servers.iter().map(|(_, server)| server)),
                &server_summary(&servers),
            )],
            None => Vec::new(),
        }
    }

    /// 声明 `/mcp` 斜杠命令及其子命令；标记 `busy_safe` 是因为 handler 只读配置/缓存
    /// 或起后台任务，不会锁住 agent。
    fn commands(&self) -> Vec<ExtensionCommand> {
        vec![ExtensionCommand {
            name: CMD.to_string(),
            description:
                "/mcp status|list|tools|test|login|logout|reload|enable|disable — manage MCP servers"
                .to_string(),
            busy_safe: true, // handler 只读配置/缓存或起后台任务，不锁 agent
            subcommands: SUBCOMMANDS.to_vec(),
        }]
    }

    /// 注册 `/mcp` 命令的执行入口。
    fn on_registered(&self) {
        register_slash_command(EXT, CMD, command_mcp);
    }

    /// 启用状态变化时丢弃管理器缓存，使下次调用按新配置重建连接。
    fn on_enabled_changed(&self, _enabled: bool) {
        invalidate_manager();
    }

    /// MCP 启用时：配置里存在 `codemode` 系暴露的服务器就自动开启 codemode 扩展
    fn on_session_start(&self, cwd: &str, _messages: &[AgentMessage], _session: Option<&Session>) {
        auto_enable_codemode();

        if let Some(message) = startup_message(cwd) {
            notify_plain(message, UiNotifyLevel::Info);
        }
    }

    /// 仅接受工具名 `mcp`（其他名字返回错误），否则按 `action` 转交 [`dispatch`] 异步执行。
    fn execute_tool_async(
        &self,
        name: String,
        args: Value,
        ctx: ToolExecCtx,
    ) -> BoxFuture<'static, Result<ToolResult, ToolError>> {
        Box::pin(async move {
            if name != TOOL {
                return Err(ToolError(format!("mcp: unknown tool: {name}")));
            }
            dispatch(args, ctx).await
        })
    }
}

/// 当前 cwd 的服务器配置（项目配置仅在受信任时参与），含扩展声明的服务器。
fn server_entries(cwd: &str) -> BTreeMap<String, ServerEntry> {
    let path = Path::new(if cwd.is_empty() { "." } else { cwd });
    let trusted = is_project_trusted(path, &agent_dir());
    load_config_with_declarations(path, trusted)
        .map(|loaded| loaded.config.mcp_servers)
        .unwrap_or_default()
}

/// 该服务器工具在当前上下文是否可见可调。
///
/// - `hidden`：任何调用方都看不到；
/// - `codemode` / `codemode-deferred`：只有脚本宿主（`codemode`）调起时可见；
/// - `direct` / `deferred`：可见（`mcp` 工具自身的 exposure 已按服务器聚合）。
///
/// 配置里没有该服务器（已删除/未加载）时不隐藏：连接层会给出更准确的错误。
fn tool_visible(
    servers: &BTreeMap<String, ServerEntry>,
    script_call: bool,
    server: &str,
    tool: &str,
) -> bool {
    match servers.get(server).map(|entry| entry.exposure_for(tool)) {
        Some(config::McpExposure::Hidden) => false,
        Some(config::McpExposure::Codemode | config::McpExposure::CodemodeDeferred) => script_call,
        _ => true,
    }
}

/// 不可见的工具直接拒绝（`describe` / `call` 用），错误里带上配置的 exposure。
fn require_visible(
    servers: &BTreeMap<String, ServerEntry>,
    script_call: bool,
    server: &str,
    tool: &str,
) -> Result<(), ToolError> {
    if tool_visible(servers, script_call, server, tool) {
        return Ok(());
    }

    let exposure = servers
        .get(server)
        .map(|entry| entry.exposure_for(tool).as_str())
        .unwrap_or("unknown");

    Err(ToolError(format!(
        "MCP tool `{server}/{tool}` is not visible in this context (exposure: {exposure})"
    )))
}

/// 启动提示行：`MCP: <enabled> servers enabled`，项目存在被信任拦截的已启用服务器时
/// 追加 `(<blocked> blocked by project trust)`。无任何服务器（enabled 与 blocked 均为 0）时返回 `None`。
fn startup_message(cwd: &str) -> Option<String> {
    let cwd = PathBuf::from(if cwd.is_empty() { "." } else { cwd });
    let trusted = is_project_trusted(&cwd, &agent_dir());
    let enabled = load_config_with_declarations(&cwd, trusted)
        .ok()?
        .enabled_server_count();

    let blocked = if trusted {
        0
    } else {
        // 以「项目受信任」重载一次，差值即被信任拦截的已启用服务器数。
        load_config_with_declarations(&cwd, true)
            .map(|all| all.enabled_server_count().saturating_sub(enabled))
            .unwrap_or(0)
    };

    if enabled == 0 && blocked == 0 {
        return None;
    }

    Some(if blocked == 0 {
        format!("MCP: {enabled} servers enabled")
    } else {
        format!("MCP: {enabled} servers enabled ({blocked} blocked by project trust)")
    })
}

/// 按 `autoEnableCodemode`（默认开启）在有 `codemode` 系服务器时自动启用 codemode 扩展。
///
/// 只把扩展从「未启用」改为「启用」；用户显式禁用过 codemode 时不覆盖（`is_extension_enabled`
/// 只看当前启用态，因此这里只处理未启用的情况，且不写回 settings，重启后仍由用户设置决定）。
fn auto_enable_codemode() {
    let cwd = cwd();
    let trusted = is_project_trusted(&cwd, &agent_dir());
    let Ok(loaded) = load_config_with_declarations(&cwd, trusted) else {
        return;
    };

    if loaded.config.auto_enable_codemode == Some(false) {
        return;
    }

    let wants_codemode = loaded
        .config
        .mcp_servers
        .values()
        .any(|server| !server.disabled && server.uses_codemode_exposure());

    if !wants_codemode {
        return;
    }

    if !extensions::is_extension_enabled("codemode") {
        extensions::set_extension_enabled("codemode", true);
    }
}

/// 当前 cwd 已启用的 MCP 服务器条目（未配置任何服务器时为 `None`）。
fn enabled_servers() -> Option<Vec<(String, ServerEntry)>> {
    let cwd = cwd();
    let trusted = is_project_trusted(&cwd, &agent_dir());
    let loaded = load_config_with_declarations(&cwd, trusted).ok()?;
    let servers: Vec<(String, ServerEntry)> = loaded
        .config
        .mcp_servers
        .iter()
        .filter(|(_, server)| !server.disabled)
        .map(|(name, server)| (name.clone(), server.clone()))
        .collect();

    if servers.is_empty() {
        None
    } else {
        Some(servers)
    }
}

/// 系统提示 `mcp_servers` 段：列出工具**不直接声明**的已启用服务器。
///
/// 这些服务器的工具模型看不到声明（`codemode` 与 `deferred` exposure），
/// 因此靠这段告诉模型“有这么几台服务器、它们的工具怎么拿到”（脚本里 `searchTools()` /
/// `describeNamespace()`，或 `tool_search` 加载）。全部服务器都是直接声明（默认的
/// `direct`）或 MCP 扩展未启用时返回 `None`——那段就没必要占提示词。
///
/// 每台服务器一行 `- <名字> (<获取方式>)`，写了 `description` 的再附一句摘要；
/// 按名字排序，总长超过 [`MAX_SERVERS_SECTION_CHARS`] 时从末尾截断并补计数行。
pub fn servers_section() -> Option<String> {
    if !extensions::is_extension_enabled(EXT) {
        return None;
    }

    render_servers_section(&enabled_servers()?)
}

/// 由一份服务器列表渲染 `mcp_servers` 段（[`servers_section`] 的纯函数部分）。
///
/// 只保留工具不直接声明的服务器（[`has_indirect_tools`]），按名字排序；一台都没有时返回 `None`。
fn render_servers_section(servers: &[(String, ServerEntry)]) -> Option<String> {
    let mut listed: Vec<(String, ServerEntry)> = servers
        .iter()
        .filter(|(_, server)| has_indirect_tools(server))
        .cloned()
        .collect();
    if listed.is_empty() {
        return None;
    }
    listed.sort_by(|a, b| a.0.cmp(&b.0));

    let reaches: Vec<&str> = listed
        .iter()
        .map(|(_, server)| {
            if server.uses_codemode_exposure() {
                "codemode"
            } else {
                "tool_search"
            }
        })
        .collect();
    let mut intro = "MCP servers whose tools are not declared to you.".to_string();
    if reaches.contains(&"codemode") {
        intro.push_str(" Call the tools of `codemode` servers from codemode scripts.");
    }
    if reaches.contains(&"tool_search") {
        intro.push_str(" Load the tools of `tool_search` servers with `tool_search`.");
    }

    let head = |index: usize| format!("- {} ({})", listed[index].0, reaches[index]);

    // 先按「不含摘要」的宽度定下能列几台（摘要只从天剩下的预算里分）
    let size = |kept: usize| {
        let mut parts = vec![intro.clone()];
        parts.extend((0..kept).map(head));
        parts.extend(omitted_line(listed.len() - kept));
        parts.join("\n").chars().count()
    };

    let mut kept = listed.len();
    while kept > 0 && size(kept) > MAX_SERVERS_SECTION_CHARS {
        kept -= 1;
    }

    if kept == 0 {
        return None;
    }

    // 每台摘要的字符预算：`": "` 分隔符也算，且不超过单台上限
    let per_server = ((MAX_SERVERS_SECTION_CHARS.saturating_sub(size(kept)) / kept)
        .saturating_sub(2))
    .min(MAX_SERVER_DESCRIPTION_CHARS);

    let mut lines: Vec<String> = Vec::with_capacity(kept);
    for (index, head) in (0..kept).map(head).enumerate() {
        let summary = listed[index]
            .1
            .description
            .as_deref()
            .map(first_line)
            .filter(|s| !s.is_empty() && per_server > 0)
            .map(|s| truncate_chars(&s, per_server))
            .unwrap_or_default();
        lines.push(if summary.is_empty() {
            head
        } else {
            format!("{head}: {summary}")
        });
    }

    let mut parts = vec![intro];
    parts.extend(lines);
    parts.extend(omitted_line(listed.len() - kept));
    Some(format!(
        "<{MCP_SERVERS_SECTION}>\n{}\n</{MCP_SERVERS_SECTION}>",
        parts.join("\n")
    ))
}

/// 该服务器的工具是否不直接声明（`codemode` 或 `deferred` exposure）。
fn has_indirect_tools(server: &ServerEntry) -> bool {
    let indirect = |exposure: McpExposure| {
        matches!(
            exposure,
            McpExposure::Codemode | McpExposure::CodemodeDeferred | McpExposure::Deferred
        )
    };

    indirect(server.exposure.unwrap_or_default())
        || server.tool_exposure.values().any(|e| indirect(*e))
}

/// 被省略的服务器计数行；没省略时返回空。
fn omitted_line(omitted: usize) -> Option<String> {
    (omitted > 0).then(|| {
        format!(
            "- … {omitted} more server{}; find their tools with searchTools()",
            if omitted == 1 { "" } else { "s" }
        )
    })
}

/// 只取第一行并去空白：服务器说明可能带多行，段落里只放一句。
fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or_default().trim().to_string()
}

/// 截断到 `max_chars` 个字符（不是字节），超出时以 `…` 结尾。
fn truncate_chars(text: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    if text.chars().count() <= max_chars {
        return text.to_string();
    }

    let head: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{head}{DEF_ELLIPSIS}")
}

/// 服务器说明摘要（`name — description`），只列写了 `description` 的服务器。
///
/// 拼进 `mcp` 工具的说明与 snippet：进模型可见的工具描述。
/// 单条说明截断到 [`SERVER_DESCRIPTION_CHARS`] 字符，免得几台服务器把工具描述顶爆。
fn server_summary(servers: &[(String, ServerEntry)]) -> String {
    let described: Vec<String> = servers
        .iter()
        .filter_map(|(name, server)| {
            let description = server.description.as_deref()?.trim();
            if description.is_empty() {
                return None;
            }

            Some(format!(
                "{name} — {}",
                description
                    .chars()
                    .take(SERVER_DESCRIPTION_CHARS)
                    .collect::<String>()
            ))
        })
        .collect();

    if described.is_empty() {
        String::new()
    } else {
        format!("Configured servers: {}", described.join("; "))
    }
}

/// `mcp` 聚合工具的暴露方式：取已启用服务器里**最可见**的一个
/// （`direct` > `deferred` > `codemode` > `hidden`）。
///
/// 把 MCP 工具聚合成单个 `mcp` 工具（模型通过 `action` 调用），因此服务器级 exposure
/// 只能聚合成一个工具级 exposure；逐工具的 `toolExposure` 由 `mcp` 工具在 list/search/call
/// 时按调用来源过滤（见 [`visible_in_context`]）。
fn aggregate_exposure<'a>(servers: impl IntoIterator<Item = &'a ServerEntry>) -> ToolExposure {
    let rank = |exposure: McpExposure| match exposure {
        McpExposure::Direct => 0,
        McpExposure::Deferred => 1,
        McpExposure::Codemode | McpExposure::CodemodeDeferred => 2,
        McpExposure::Hidden => 3,
    };
    let least = servers
        .into_iter()
        .map(|server| server.exposure.unwrap_or_default())
        .min_by_key(|exposure| rank(*exposure))
        .unwrap_or_default();
    match least {
        McpExposure::Direct => ToolExposure::Direct,
        McpExposure::Deferred => ToolExposure::Deferred,
        McpExposure::Codemode | McpExposure::CodemodeDeferred => ToolExposure::Codemode,
        McpExposure::Hidden => ToolExposure::Hidden,
    }
}

/// 模型可见的 `mcp` 工具定义；`exposure` 由服务器配置聚合得出，
/// `summary` 是服务器 `description` 摘要（空串表示没有服务器写说明）。
fn mcp_tool(exposure: ToolExposure, summary: &str) -> ExtensionTool {
    let mut description = "Find and use tools, resources, and prompts from configured MCP servers. \
                           Start with search or list. Use describe before a tool call when you do not \
                           know its input schema."
        .to_string();
    let mut snippet = "Use `mcp` to search/list/describe/call tools, resources, and prompts from configured MCP servers.".to_string();

    if !summary.is_empty() {
        description.push_str("\n\n");
        description.push_str(summary);
        snippet.push(' ');
        snippet.push_str(summary);
    }

    ExtensionTool {
        exposure,
        namespace: None,
        annotations: ToolAnnotations::default(),
        output_schema: None,
        name: TOOL.to_string(),
        description,
        label: Some("MCP".to_string()),
        parameters: json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["status", "list", "search", "describe", "call", "resources", "read_resource", "prompts", "get_prompt"],
                    "description": "The MCP operation."
                },
                "server": {"type": "string", "description": "The configured MCP server name."},
                "name": {"type": "string", "description": "The tool or prompt name."},
                "query": {"type": "string", "description": "Text used to search tool names and descriptions."},
                "uri": {"type": "string", "description": "The resource URI."},
                "arguments": {"type": "object", "description": "Arguments for a tool or prompt."}
            },
            "required": ["action"],
            "additionalProperties": false
        }),
        snippet,
        prompt_guidelines: Vec::new(),
        constrained_sampling: false,
        render_shell: None,
        execution_mode: Some(ToolExecutionMode::Parallel),
        prepare_arguments: None,
        grammar_sampling: None,
    }
}

/// 工具分发：按 `action` 执行对应 MCP 操作。
async fn dispatch(args: Value, ctx: ToolExecCtx) -> Result<ToolResult, ToolError> {
    let action = required_string(&args, "action")?.to_string();
    let manager = manager_for(&ctx.cwd)?;
    let cancel = CancellationToken::new();
    let abort = ctx.parent_abort.clone();

    // 逐工具的 `toolExposure` 过滤：`codemode` 系的工具只对脚本调用可见
    let servers = server_entries(&ctx.cwd);
    let script_call = ctx.script_call;

    match action.as_str() {
        "status" => json_result(manager.status().await),
        "list" => {
            let server = optional_string(&args, "server").map(str::to_string);
            let mut tools = run_cancellable(
                &cancel,
                abort,
                manager.list_tools(server.as_deref(), &cancel),
            )
            .await
            .map_err(tool_error)?;
            tools.retain(|t| tool_visible(&servers, script_call, &t.server, &t.name));
            json_result(tools)
        }
        "search" => {
            let query = required_string(&args, "query")?.to_string();
            let server = optional_string(&args, "server").map(str::to_string);
            let mut tools = run_cancellable(
                &cancel,
                abort,
                manager.search_tools(&query, server.as_deref(), &cancel),
            )
            .await
            .map_err(tool_error)?;
            tools.retain(|t| tool_visible(&servers, script_call, &t.server, &t.name));
            json_result(tools)
        }
        "describe" => {
            let server = required_string(&args, "server")?.to_string();
            let name = required_string(&args, "name")?.to_string();
            require_visible(&servers, script_call, &server, &name)?;
            let tool = run_cancellable(
                &cancel,
                abort,
                manager.describe_tool(&server, &name, &cancel),
            )
            .await
            .map_err(tool_error)?;
            json_result(tool)
        }
        "call" => {
            let server = required_string(&args, "server")?.to_string();
            let name = required_string(&args, "name")?.to_string();
            require_visible(&servers, script_call, &server, &name)?;
            let arguments = args.get("arguments").cloned().unwrap_or(Value::Null);
            let result = run_cancellable(
                &cancel,
                abort,
                manager.call_tool(&server, &name, arguments, &cancel),
            )
            .await
            .map_err(tool_error)?;

            let is_error = result.is_error.unwrap_or(false);
            let details = json!({
                "action": action,
                "server": server,
                "name": name,
                "isError": is_error,
                "meta": result.meta,
            });
            let (text, attachments) = convert_content(result.content);

            if is_error {
                return Err(ToolError(format!(
                    "MCP tool `{server}/{name}` failed: {text}"
                )));
            }

            Ok(ToolResult {
                text,
                structured_content: result.structured_content,
                details: Some(details),
                attachments,
                ..Default::default()
            })
        }
        "resources" => {
            let server = required_string(&args, "server")?.to_string();
            let resources =
                run_cancellable(&cancel, abort, manager.list_resources(&server, &cancel))
                    .await
                    .map_err(tool_error)?;
            json_result(resources)
        }
        "read_resource" => {
            let server = required_string(&args, "server")?.to_string();
            let uri = required_string(&args, "uri")?.to_string();
            let value = run_cancellable(
                &cancel,
                abort,
                manager.read_resource(&server, &uri, &cancel),
            )
            .await
            .map_err(tool_error)?;
            json_result(value)
        }
        "prompts" => {
            let server = required_string(&args, "server")?.to_string();
            let prompts = run_cancellable(&cancel, abort, manager.list_prompts(&server, &cancel))
                .await
                .map_err(tool_error)?;
            json_result(prompts)
        }
        "get_prompt" => {
            let server = required_string(&args, "server")?.to_string();
            let name = required_string(&args, "name")?.to_string();
            let arguments = args.get("arguments").cloned().unwrap_or(Value::Null);
            let value = run_cancellable(
                &cancel,
                abort,
                manager.get_prompt(&server, &name, arguments, &cancel),
            )
            .await
            .map_err(tool_error)?;
            json_result(value)
        }
        other => Err(ToolError(format!("unknown MCP action `{other}`"))),
    }
}

/// 竞争「请求完成」与「父级取消」：取消时置位 token 并等待请求优雅返回。
async fn run_cancellable<F, T>(
    cancel: &CancellationToken,
    abort: Arc<AtomicBool>,
    future: F,
) -> Result<T, McpError>
where
    F: Future<Output = Result<T, McpError>>,
{
    let mut future = Box::pin(future);
    tokio::select! {
        result = &mut future => result,
        _ = wait_abort(abort) => {
            cancel.cancel();
            future.await
        }
    }
}

/// 每 50ms 轮询父级 abort 标志。
async fn wait_abort(abort: Arc<AtomicBool>) {
    loop {
        if abort.load(Ordering::Relaxed) {
            return;
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// 把 MCP 层错误转成工具错误，保留原始错误文本。
fn tool_error(error: McpError) -> ToolError {
    ToolError(error.to_string())
}

/// 取必填的字符串参数；缺失、非字符串或为空串时返回 `ToolError`（错误信息指明缺少哪个 key）。
fn required_string<'a>(args: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ToolError(format!("MCP action needs `{key}`")))
}

/// 取可选字符串参数：缺失、非字符串或为空串均视为 `None`。
fn optional_string<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

/// 把任意可序列化结果渲染成 `ToolResult`（文本 + details）。
fn json_result(value: impl Serialize) -> Result<ToolResult, ToolError> {
    let details = serde_json::to_value(&value).map_err(|e| ToolError(e.to_string()))?;
    let text = serde_json::to_string_pretty(&value).map_err(|e| ToolError(e.to_string()))?;
    Ok(ToolResult {
        text: limit_text(text),
        details: Some(details),
        ..Default::default()
    })
}

/// MCP 内容块 → 文本 + 图片附件。
fn convert_content(content: Vec<McpContent>) -> (String, Vec<ToolResultAttachment>) {
    let mut texts: Vec<String> = Vec::with_capacity(content.len());
    let mut attachments = Vec::new();

    for block in content {
        match block {
            McpContent::Text(text) => texts.push(limit_text(text.text)),
            McpContent::Image(image) => attachments.push(ToolResultAttachment {
                data_base64: image.data,
                mime_type: image.mime_type,
                original_size: None,
                converted_from: None,
            }),
            McpContent::Audio(audio) => texts.push(format!(
                "[MCP audio content: {}; {} base64 bytes]",
                audio.mime_type,
                audio.data.len()
            )),
            McpContent::Resource(resource) => {
                texts.push(limit_text(
                    serde_json::to_string_pretty(&resource).unwrap_or_default(),
                ));
            }
            McpContent::ResourceLink(resource) => {
                texts.push(format!(
                    "MCP resource: {} ({})",
                    resource.name, resource.uri
                ));
            }
            other => {
                texts.push(limit_text(
                    serde_json::to_string_pretty(&other).unwrap_or_default(),
                ));
            }
        }
    }

    if texts.is_empty() && attachments.is_empty() {
        texts.push("MCP tool completed with no content.".to_string());
    }

    let mut parts = Vec::new();
    for attachment in &attachments {
        parts.push(format!("[image: {}]", attachment.mime_type));
    }

    parts.extend(texts);
    (parts.join("\n"), attachments)
}

/// 把超长文本截断到 `MAX_TEXT_BYTES`（按 UTF-8 字符边界回退），并追加一行截断说明；
/// 未超长时原样返回。
fn limit_text(mut text: String) -> String {
    if text.len() <= MAX_TEXT_BYTES {
        return text;
    }

    let mut end = MAX_TEXT_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }

    text.truncate(end);
    text.push_str(&format!("\n\n[MCP output was truncated by {APP_NAME}.]\n"));
    text
}

/// MCP 服务器名（供命令侧做参数校验 / 展示）。
fn lookup_server<'a>(loaded: &'a LoadedConfig, name: &str) -> Result<&'a ServerEntry, String> {
    loaded
        .config
        .mcp_servers
        .get(name)
        .ok_or_else(|| format!("MCP server `{name}` is not configured"))
}

/// 查找名为 `name` 的服务器并确认它是 HTTP 传输（配置了 `url`）；
/// 服务器不存在或为 stdio 时返回 `Err` 描述。
fn http_server<'a>(loaded: &'a LoadedConfig, name: &str) -> Result<&'a ServerEntry, String> {
    let server = lookup_server(loaded, name)?;
    if server.url.is_none() {
        return Err(format!(
            "MCP server `{name}` is a stdio server and does not use OAuth"
        ));
    }
    Ok(server)
}

/// 当前 cwd 的合并配置（项目信任按现有决策判定），含扩展声明的服务器。
fn load_config() -> Result<LoadedConfig, String> {
    let cwd = cwd();
    let trusted = is_project_trusted(&cwd, &agent_dir());
    load_config_with_declarations(&cwd, trusted)
}

/// 合并配置 + **扩展声明的服务器**
///
/// 扩展侧的每条加载路径都走这里：声明是按会话生效、不落盘的，因此只能在
/// 配置加载后叠上去；`mcp.json` 同名条目优先
/// （见 [`config::apply_declared_servers`]）。CLI（`prux mcp ...`）不加载扩展，仍用 [`config::load`]，
fn load_config_with_declarations(cwd: &Path, trusted: bool) -> Result<LoadedConfig, String> {
    let mut loaded = config::load(cwd, trusted).map_err(|e| e.to_string())?;
    let errors = config::apply_declared_servers(&mut loaded, extensions::declared_mcp_servers());
    if !errors.is_empty() {
        notify_plain(errors.join("; "), UiNotifyLevel::Warning);
    }
    Ok(loaded)
}

/// `/mcp` 命令入口。
fn command_mcp(st: &mut App, raw: &str) -> bool {
    let arg = command_arg(raw).trim();
    let (sub, rest) = match arg.split_once(char::is_whitespace) {
        Some((a, b)) => (a, b.trim()),
        None => (arg, ""),
    };

    match sub {
        "" | "status" => {
            st.push_msg(render_status(), MsgLevel::Info);
        }
        "list" => {
            st.push_msg(render_list(), MsgLevel::Info);
        }
        "tools" => {
            let server = (!rest.is_empty()).then_some(rest);
            st.push_msg(render_cached_tools(server), MsgLevel::Info);
        }
        "reload" => {
            invalidate_manager();
            extensions::request_ui(ExtensionUiRequest::RebuildTools);
            st.push_msg(
                "MCP config reloaded; cached connections dropped.".to_string(),
                MsgLevel::Success,
            );
        }
        "enable" | "disable" => {
            let enabled = sub == "enable";
            let mut parts = rest.split_whitespace();
            let Some(server) = parts.next() else {
                st.push_msg(
                    format!("Usage: /mcp {sub} <server> [user|project]"),
                    MsgLevel::Warning,
                );
                return false;
            };

            let scope = match parts.next() {
                None | Some("project") => config::ConfigScope::Project,
                Some("user") => config::ConfigScope::User,
                Some(other) => {
                    st.push_msg(
                        format!("Unknown scope `{other}`; use user or project."),
                        MsgLevel::Warning,
                    );
                    return false;
                }
            };

            let loaded = match load_config() {
                Ok(loaded) => loaded,
                Err(error) => {
                    st.push_msg(format!("MCP config error: {error}"), MsgLevel::Error);
                    return false;
                }
            };

            if !loaded.config.mcp_servers.contains_key(server) {
                st.push_msg(
                    format!("MCP server `{server}` is not configured."),
                    MsgLevel::Warning,
                );
                return false;
            }

            if let Err(error) = config::set_disabled(&loaded.paths, scope, server, !enabled) {
                st.push_msg(format!("MCP config error: {error}"), MsgLevel::Error);
                return false;
            }

            invalidate_manager();
            extensions::request_ui(ExtensionUiRequest::RebuildTools);
            let scope_label = match scope {
                config::ConfigScope::User => "user config",
                config::ConfigScope::Project => "project config",
            };

            if scope == config::ConfigScope::Project && !is_project_trusted(&cwd(), &agent_dir()) {
                st.push_msg(
                    "This project is not trusted; the override will not take effect until you approve it."
                        .to_string(),
                    MsgLevel::Warning,
                );
            }

            st.push_msg(
                format!(
                    "MCP server `{server}` {} in {scope_label}.",
                    if enabled { "enabled" } else { "disabled" }
                ),
                MsgLevel::Success,
            );
        }
        "test" => {
            if rest.is_empty() {
                st.push_msg("Usage: /mcp test <server>".to_string(), MsgLevel::Warning);
                return false;
            }
            spawn_test(rest.to_string());
            st.push_msg(
                format!("Connecting to MCP server `{rest}`…"),
                MsgLevel::Info,
            );
        }
        "logout" => {
            if rest.is_empty() {
                st.push_msg("Usage: /mcp logout <server>".to_string(), MsgLevel::Warning);
                return false;
            }
            spawn_logout(rest.to_string());
            st.push_msg(
                format!("Removing MCP OAuth credentials for `{rest}`…"),
                MsgLevel::Info,
            );
        }
        "login" => {
            let (server, callback) = match rest.split_once(char::is_whitespace) {
                Some((s, c)) => (s.trim(), Some(c.trim().to_string())),
                None => (rest, None),
            };
            if server.is_empty() {
                st.push_msg(
                    "Usage: /mcp login <server> [redirect-url]".to_string(),
                    MsgLevel::Warning,
                );
                return false;
            }
            match http_server(&load_config().unwrap_or_else(|_| empty_loaded()), server) {
                Ok(entry) => {
                    if entry.auth_mode() != Some(AuthMode::OAuth) {
                        st.push_msg(
                            format!(
                                "MCP server `{server}` does not use OAuth (set auth=\"oauth\" or an oauth block)."
                            ),
                            MsgLevel::Warning,
                        );
                        return false;
                    }
                }
                Err(error) => {
                    st.push_msg(error, MsgLevel::Error);
                    return false;
                }
            }
            spawn_login(server.to_string(), callback);
            st.push_msg(
                format!("Starting MCP OAuth login for `{server}`…"),
                MsgLevel::Info,
            );
        }
        other => {
            st.push_msg(
                format!("Unknown MCP subcommand `{other}`. Usage: /mcp [{SUBCOMMAND_NAMES}]"),
                MsgLevel::Warning,
            );
        }
    }
    false
}

/// 构造空配置的 `LoadedConfig`，用于读配置失败时继续做参数校验（路径仍按当前 cwd 探测）。
fn empty_loaded() -> LoadedConfig {
    let mut loaded = LoadedConfig {
        config: config::McpConfig::default(),
        sources: Default::default(),
        paths: config::ConfigPaths::discover(&cwd()),
    };

    // 读配置失败也要校验扩展声明的服务器（参数校验与是否连得上无关）
    _ = config::apply_declared_servers(&mut loaded, extensions::declared_mcp_servers());
    loaded
}

/// 状态行：配置 + 缓存计数（不连接）。
fn render_status() -> String {
    let loaded = match load_config() {
        Ok(loaded) => loaded,
        Err(error) => return format!("MCP config error: {error}"),
    };

    if loaded.config.mcp_servers.is_empty() {
        return "No MCP servers are configured. Add one to ~/.agents/mcp.json or agent_dir()/extensions/mcp/mcp.json (project .mcp.json also works when trusted).".to_string();
    }

    let cache = manager::cache_counts(&config::cache_path());
    let mut lines = vec!["MCP servers:".to_string()];
    for (name, server) in &loaded.config.mcp_servers {
        let transport = server
            .url
            .as_deref()
            .map(|url| format!("http {url}"))
            .unwrap_or_else(|| format!("stdio {}", server.command.as_deref().unwrap_or("")));

        let state = if server.disabled {
            "disabled"
        } else {
            "enabled"
        };

        let counts = cache.get(name).copied().unwrap_or_default();

        // 暴露方式（含逐工具覆盖的差异）：`direct` 之外的取值会影响模型可见性
        let exposure = server.exposure.unwrap_or_default().as_str();
        let overrides: Vec<String> = server
            .tool_exposure
            .iter()
            .map(|(tool, value)| format!("{tool}={}", value.as_str()))
            .collect();
        let overrides = if overrides.is_empty() {
            String::new()
        } else {
            format!(" [{}]", overrides.join(" "))
        };

        lines.push(format!(
            "  {name}\t{state}\t{transport}\texposure={exposure}{overrides}\ttools={} resources={} prompts={}{}",
            counts.0,
            counts.1,
            counts.2,
            described(server)
        ));
    }

    lines.push("Use the `mcp` tool (or `/mcp test <server>`) to connect and refresh.".to_string());
    lines.join("\n")
}

/// 配置列表：服务器 + 来源。
fn render_list() -> String {
    let loaded = match load_config() {
        Ok(loaded) => loaded,
        Err(error) => return format!("MCP config error: {error}"),
    };

    if loaded.config.mcp_servers.is_empty() {
        return "No MCP servers are configured.".to_string();
    }

    let mut lines = Vec::new();
    for (name, server) in &loaded.config.mcp_servers {
        let transport = server
            .url
            .as_deref()
            .map(|url| format!("http {url}"))
            .unwrap_or_else(|| format!("stdio {}", server.command.as_deref().unwrap_or("")));
        lines.push(format!(
            "{name} [{}] {transport}{}",
            if server.disabled {
                "disabled"
            } else {
                "enabled"
            },
            described(server)
        ));
        for source in loaded.source_labels(name) {
            lines.push(format!("    from {source}"));
        }
    }
    lines.join("\n")
}

/// `/mcp` 列表里的服务器说明后缀（没写 `description` 时为空）。
fn described(server: &ServerEntry) -> String {
    match server.description.as_deref().map(str::trim) {
        Some(description) if !description.is_empty() => format!("\t{description}"),
        _ => String::new(),
    }
}

/// 缓存工具列表（不连接）。
fn render_cached_tools(server: Option<&str>) -> String {
    let cache = manager::cache_entries(&config::cache_path());
    let mut tools: Vec<CachedTool> = cache
        .into_values()
        .flatten()
        .filter(|tool| server.is_none_or(|name| tool.server == name))
        .collect();

    if tools.is_empty() {
        return match server {
            Some(name) => format!(
                "No cached tools for `{name}`. Run `/mcp test {name}` or use the `mcp` tool's `list` action."
            ),
            None => "No cached MCP tools yet. Use the `mcp` tool's `list`/`search` action to populate the cache.".to_string(),
        };
    }

    tools.sort_by_key(|tool| (tool.server.clone(), tool.name.clone()));
    tools
        .into_iter()
        .map(|tool| {
            format!(
                "{}/{} — {}",
                tool.server,
                tool.name,
                tool.description.as_deref().unwrap_or("(no description)")
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 后台连接指定服务器并拉取工具列表，完成后用通知回执工具数或失败原因。
fn spawn_test(server: String) {
    tokio::spawn(async move {
        let cwd = cwd();
        let result = async {
            let trusted = is_project_trusted(&cwd, &agent_dir());
            let loaded = load_config_with_declarations(&cwd, trusted)?;

            lookup_server(&loaded, &server)?;

            let manager = McpManager::new(loaded).map_err(|e| e.to_string())?;
            let cancel = CancellationToken::new();
            let tools = manager
                .list_tools(Some(&server), &cancel)
                .await
                .map_err(|e| e.to_string())?;

            manager.disconnect_all().await;
            Ok::<usize, String>(tools.len())
        }
        .await;

        match result {
            Ok(count) => notify_plain(
                format!("Connected to MCP server `{server}`. Found {count} tools."),
                UiNotifyLevel::Success,
            ),
            Err(error) => notify_plain(
                format!("MCP server `{server}` failed: {error}"),
                UiNotifyLevel::Error,
            ),
        }
    });
}

/// 后台删除指定 HTTP 服务器的 OAuth 凭据，并通知是确实删除了还是本来就没有。
fn spawn_logout(server: String) {
    tokio::spawn(async move {
        let result = async {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            let trusted = is_project_trusted(&cwd, &agent_dir());
            let loaded = load_config_with_declarations(&cwd, trusted)?;
            let entry = http_server(&loaded, &server)?;
            let url = entry
                .url
                .clone()
                .ok_or_else(|| "MCP URL is missing".to_string())?;
            let existed = oauth::logout(&server, &url)
                .await
                .map_err(|e| e.to_string())?;
            Ok::<bool, String>(existed)
        }
        .await;

        match result {
            Ok(true) => notify_plain(
                format!("Removed MCP OAuth credentials for `{server}`."),
                UiNotifyLevel::Success,
            ),
            Ok(false) => notify_plain(
                format!("No MCP OAuth credentials are saved for `{server}`."),
                UiNotifyLevel::Info,
            ),
            Err(error) => notify_plain(error, UiNotifyLevel::Error),
        }
    });
}

/// 后台执行 OAuth 登录：传了回调 URL 时走手动完成路径，否则打开浏览器 + 本地回调监听。
/// 结果（成功或错误）经通知回执展示。
///
/// 登录登记到 [`core::extensions::register_background_cancel`]：Esc / Ctrl+C 与会话关闭会取消它
fn spawn_login(server: String, callback: Option<String>) {
    let cancel = CancellationToken::new();
    let _guard =
        extensions::register_background_cancel(format!("mcp-login:{server}"), cancel.clone());

    tokio::spawn(async move {
        let result = match callback {
            // 用户手动粘贴回调 URL：重新构造一次 pending 并完成。
            Some(callback) => finish_login_from_url(&server, &callback).await,
            None => login_with_browser(&server, &cancel).await,
        };

        if cancel.is_cancelled() {
            notify_plain(
                format!("MCP OAuth sign-in for `{server}` was cancelled."),
                UiNotifyLevel::Info,
            );
            return;
        }

        match result {
            Ok(()) => notify_plain(
                format!("Saved OAuth credentials for MCP server `{server}`."),
                UiNotifyLevel::Success,
            ),
            Err(error) => notify_plain(error, UiNotifyLevel::Error),
        }
    });
}

/// 浏览器式 OAuth 登录：`client_credentials` 直接换令牌；否则探测授权挑战、
/// 起本地回调监听并打开浏览器，收到回调后换取凭据。
/// 任一步失败（超时、无法监听、回调解析失败等）返回 `Err` 文本。
async fn login_with_browser(server: &str, cancel: &CancellationToken) -> Result<(), String> {
    let cwd = cwd();
    let trusted = is_project_trusted(&cwd, &agent_dir());
    let loaded = load_config_with_declarations(&cwd, trusted)?;
    let entry = http_server(&loaded, server)?;
    let url = entry
        .url
        .clone()
        .ok_or_else(|| "MCP URL is missing".to_string())?;
    let oauth_config = entry.oauth.clone().unwrap_or_default();

    if oauth_config.grant_type == config::OAuthGrantType::ClientCredentials {
        return oauth::login_client_credentials_with_cancel(
            server,
            &url,
            &oauth_config,
            None,
            Some(cancel.clone()),
        )
        .await
        .map_err(|e| e.to_string());
    }

    let challenge = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err("MCP OAuth sign-in was cancelled".to_string()),
        result = tokio::time::timeout(
            Duration::from_secs(15),
            manager::probe_oauth_challenge(entry),
        ) => result,
    }
    .map_err(|_| "MCP OAuth discovery timed out".to_string())?
    .map_err(|e| e.to_string())?;

    let pending = oauth::begin_login_with_cancel(
        server,
        &url,
        &oauth_config,
        challenge.as_deref(),
        None,
        Some(cancel.clone()),
    )
    .await
    .map_err(|e| e.to_string())?;

    let redirect_uri = pending.redirect_uri.clone();
    notify_plain(
        format!(
            "Open this URL to sign in to `{server}`:\n{}",
            pending.authorization_url
        ),
        UiNotifyLevel::Info,
    );

    let Some(listener) = callback_listener(&redirect_uri).await? else {
        return Err(format!(
            "Cannot listen on `{redirect_uri}` for the OAuth callback. Re-run as `/mcp login {server} <redirect-url>` after signing in."
        ));
    };

    if webbrowser::open(&pending.authorization_url).is_err() {
        notify_plain(
            "The browser did not open. Open the URL manually.",
            UiNotifyLevel::Warning,
        );
    }

    let callback = receive_callback(listener, &redirect_uri, cancel).await?;
    oauth::finish_login(pending, &callback)
        .await
        .map_err(|e| e.to_string())
}

/// 用用户手动粘贴的回调 URL 完成登录；仅 `client_credentials` 支持
/// （authorization_code 的 PKCE state 只存在内存，无法复用），其余返回 `Err` 提示改用浏览器流程。
/// `_callback` 目前未参与取凭据。
async fn finish_login_from_url(server: &str, _callback: &str) -> Result<(), String> {
    let cwd = cwd();
    let trusted = is_project_trusted(&cwd, &agent_dir());
    let loaded = load_config_with_declarations(&cwd, trusted)?;
    let entry = http_server(&loaded, server)?;
    let url = entry
        .url
        .clone()
        .ok_or_else(|| "MCP URL is missing".to_string())?;
    let oauth_config = entry.oauth.clone().unwrap_or_default();

    // 手动粘贴的回调 URL 无法复用此前的 CSRF/PKCE 状态；RMCP 的 state store 是内存态，
    // 因此这里只能走 client credentials，否则提示用浏览器流程。
    if oauth_config.grant_type == OAuthGrantType::ClientCredentials {
        return oauth::login_client_credentials(server, &url, &oauth_config)
            .await
            .map_err(|e| e.to_string());
    }

    Err(format!(
        "Manual redirect-URL completion is not supported for authorization_code (PKCE state is in-memory). Run `/mcp login {server}` and let the local callback listener complete it."
    ))
}

/// 为 OAuth 回调绑定本地监听端口：`redirect_uri` 指向 localhost/127.0.0.1/::1 时返回监听器，
/// 非本地 host 或 URL 无 host 时返回 `Ok(None)`（改由用户手动粘贴回调）；绑定失败返回 `Err`。
pub async fn callback_listener(redirect_uri: &str) -> Result<Option<TcpListener>, String> {
    let uri = url::Url::parse(redirect_uri).map_err(|e| e.to_string())?;
    let Some(host) = uri.host_str() else {
        return Ok(None);
    };

    if !matches!(host, "localhost" | "127.0.0.1" | "::1") {
        return Ok(None);
    }

    let port = uri
        .port_or_known_default()
        .ok_or_else(|| "OAuth redirect URI has no port".to_string())?;

    let address = if host == "::1" {
        format!("[::1]:{port}")
    } else {
        format!("127.0.0.1:{port}")
    };

    let listener = TcpListener::bind(address)
        .await
        .map_err(|e| format!("bind {redirect_uri}: {e}"))?;

    Ok(Some(listener))
}

/// 等待浏览器回调请求（最长 600s），解析请求行里的回调地址、回写一个 200 提示页，
/// 返回相对于 `redirect_uri` 拼好的绝对回调 URL。超时、取消或请求非法时返回 `Err`。
pub async fn receive_callback(
    listener: TcpListener,
    redirect_uri: &str,
    cancel: &CancellationToken,
) -> Result<String, String> {
    let accepted = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err("MCP OAuth sign-in was cancelled".to_string()),
        result = tokio::time::timeout(Duration::from_secs(600), listener.accept()) => result,
    };

    let (mut socket, _) = accepted
        .map_err(|_| "MCP OAuth callback timed out".to_string())?
        .map_err(|e| e.to_string())?;

    let mut buffer = vec![0_u8; 16 * 1024];
    let length = socket.read(&mut buffer).await.map_err(|e| e.to_string())?;
    let request =
        std::str::from_utf8(&buffer[..length]).map_err(|_| "invalid OAuth callback".to_string())?;
    let target = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .ok_or_else(|| "invalid OAuth callback request line".to_string())?;
    let base = Url::parse(redirect_uri).map_err(|e| e.to_string())?;
    let callback = base.join(target).map_err(|e| e.to_string())?.to_string();

    _ = socket
        .write_all(
            &format!("HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\n\r\n<!doctype html><title>{APP_NAME} MCP login</title><p>Login is complete. You can close this page.</p>").into_bytes()
        )
        .await;

    Ok(callback)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{AgentDirGuard, HomeGuard};

    /// 配置计数类断言与"扩展声明的服务器"（全局注册表）会相互干扰，用例之间串行化。
    fn lock() -> std::sync::MutexGuard<'static, ()> {
        crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// 聚合网关工具名与 core 的工具名判定必须同源：
    /// core 不能依赖 extensions，故 `MCP_GATEWAY_TOOL_NAME` 是硬编码副本，由这条测试守住。
    #[test]
    fn gateway_tool_name_matches_core_constant() {
        assert_eq!(TOOL, crate::core::tool_names::MCP_GATEWAY_TOOL_NAME);
    }

    /// pi #10565：登录在等待浏览器回调时 Esc 可立即取消，不再干等 600s。
    #[tokio::test]
    async fn receive_callback_aborts_on_cancel() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let error = receive_callback(listener, "http://127.0.0.1:1/callback", &cancel)
            .await
            .unwrap_err();
        assert!(error.contains("cancelled"), "{error}");
    }

    #[test]
    fn text_limit_keeps_utf8_valid() {
        let text = "é".repeat(MAX_TEXT_BYTES);
        let limited = limit_text(text);
        assert!(limited.is_char_boundary(limited.len()));
        assert!(limited.contains("truncated"));
    }

    /// 聚合暴露方式：取已启用服务器里最可见的一个（direct > deferred > codemode > hidden）。
    #[test]
    fn aggregate_exposure_picks_most_visible() {
        let entry = |exposure: Option<config::McpExposure>| {
            let mut server = ServerEntry {
                url: Some("https://example.com/mcp".to_string()),
                ..Default::default()
            };
            server.exposure = exposure;
            server
        };

        assert_eq!(
            aggregate_exposure(&[entry(None)]),
            ToolExposure::Direct,
            "未配置 exposure 时保持 direct（prux 既有行为）"
        );
        assert_eq!(
            aggregate_exposure(&[
                entry(Some(config::McpExposure::Codemode)),
                entry(Some(config::McpExposure::Direct)),
            ]),
            ToolExposure::Direct
        );
        assert_eq!(
            aggregate_exposure(&[
                entry(Some(config::McpExposure::Codemode)),
                entry(Some(config::McpExposure::Deferred)),
            ]),
            ToolExposure::Deferred
        );
        assert_eq!(
            aggregate_exposure(&[entry(Some(config::McpExposure::CodemodeDeferred))]),
            ToolExposure::Codemode
        );
        assert_eq!(
            aggregate_exposure(&[entry(Some(config::McpExposure::Hidden))]),
            ToolExposure::Hidden
        );
    }

    /// 逐工具暴露：`hidden` 谁都看不到；`codemode` 系只对脚本调用可见。
    #[test]
    fn tool_visibility_follows_exposure_and_caller() {
        let mut server = ServerEntry {
            url: Some("https://example.com/mcp".to_string()),
            ..Default::default()
        };
        server
            .tool_exposure
            .insert("read_*".to_string(), config::McpExposure::Hidden);
        server
            .tool_exposure
            .insert("write".to_string(), config::McpExposure::Codemode);
        let servers = BTreeMap::from([("docs".to_string(), server)]);

        assert!(tool_visible(&servers, false, "docs", "search"));
        assert!(!tool_visible(&servers, true, "docs", "read_file"));
        assert!(!tool_visible(&servers, false, "docs", "write"));
        assert!(tool_visible(&servers, true, "docs", "write"));
        // 配置里没有的服务器不隐藏（交给连接层报错）
        assert!(tool_visible(&servers, false, "other", "anything"));

        let err = require_visible(&servers, false, "docs", "write").unwrap_err();
        assert!(err.0.contains("exposure: codemode"), "got: {}", err.0);
        assert!(require_visible(&servers, true, "docs", "write").is_ok());
    }

    #[test]
    fn required_values_are_checked() {
        let input = json!({"action": "search", "query": "files"});
        assert_eq!(required_string(&input, "action").unwrap(), "search");
        assert_eq!(required_string(&input, "query").unwrap(), "files");
        assert!(required_string(&input, "server").is_err());
        assert_eq!(optional_string(&input, "server"), None);
    }

    #[test]
    fn content_conversion_marks_images_and_audio() {
        use rmcp::model::{AudioContent, ContentBlock, ImageContent, TextContent};
        let content = vec![
            ContentBlock::Text(TextContent::new("hello")),
            ContentBlock::Image(ImageContent::new("ZGF0YQ==", "image/png")),
            ContentBlock::Audio(AudioContent::new("YXVkaW8=", "audio/wav")),
        ];
        let (text, attachments) = convert_content(content);
        assert!(text.contains("hello"));
        assert!(text.contains("[image: image/png]"));
        assert!(text.contains("MCP audio content"));
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].mime_type, "image/png");
    }

    /// 2.12：服务器 `description` 摘要只列写了说明的服务器，单条超长按字符截断。
    #[test]
    fn server_summary_lists_described_servers() {
        let described = |description: Option<&str>| ServerEntry {
            description: description.map(str::to_string),
            ..Default::default()
        };
        let servers = vec![
            ("a".to_string(), described(Some("Alpha tools"))),
            ("b".to_string(), described(None)),
            ("c".to_string(), described(Some("   "))),
        ];
        assert_eq!(
            server_summary(&servers),
            "Configured servers: a — Alpha tools"
        );
        assert!(server_summary(&[("a".to_string(), described(None))]).is_empty());

        let long = "x".repeat(SERVER_DESCRIPTION_CHARS + 50);
        let summary = server_summary(&[("a".to_string(), described(Some(&long)))]);
        assert_eq!(
            summary.chars().filter(|c| *c == 'x').count(),
            SERVER_DESCRIPTION_CHARS
        );
    }

    /// 2.17：`mcp_servers` 段只列工具不直接声明的服务器（`codemode` / `deferred`），
    /// 并给出获取方式与一句摘要；全 direct 时这段不存在。
    #[test]
    fn servers_section_lists_indirect_servers() {
        let entry = |exposure: Option<McpExposure>, description: Option<&str>| ServerEntry {
            command: Some("echo".to_string()),
            exposure,
            description: description.map(str::to_string),
            ..Default::default()
        };
        let named = |name: &str, server: ServerEntry| (name.to_string(), server);

        // 默认 exposure（direct）：工具是直接声明的，这段不需要
        let direct = vec![named(
            "direct-one",
            entry(None, Some("Hidden from the section")),
        )];
        assert_eq!(render_servers_section(&direct), None);

        // codemode + deferred：两种获取方式都要在引导句里点名
        let servers = vec![
            named("direct-one", entry(None, None)),
            named(
                "zeta",
                entry(
                    Some(McpExposure::Codemode),
                    Some("Zeta tools\nsecond line is dropped"),
                ),
            ),
            named("alpha", entry(Some(McpExposure::Deferred), None)),
            // `toolExposure` 里出现间接 exposure 的服务器同样要列
            named(
                "mixed",
                ServerEntry {
                    command: Some("echo".to_string()),
                    tool_exposure: [("x".to_string(), McpExposure::Codemode)].into(),
                    ..Default::default()
                },
            ),
            // hidden：任何调用都到不了，不列
            named("hidden-one", entry(Some(McpExposure::Hidden), None)),
        ];

        let section = render_servers_section(&servers).unwrap();
        assert!(section.starts_with("<mcp_servers>\n"), "{section}");
        assert!(section.ends_with("</mcp_servers>"), "{section}");
        assert!(
            section.contains(
                "MCP servers whose tools are not declared to you. Call the tools of `codemode` servers from codemode scripts. Load the tools of `tool_search` servers with `tool_search`."
            ),
            "{section}"
        );
        assert!(section.contains("- alpha (tool_search)"), "{section}");
        // 服务器级 exposure 是默认的 direct，但 `toolExposure` 里有 codemode → 按 pi 的口径算 codemode
        assert!(section.contains("- mixed (codemode)"), "{section}");
        assert!(
            section.contains("- zeta (codemode): Zeta tools"),
            "{section}"
        );
        assert!(
            !section.contains("second line") && !section.contains("direct-one"),
            "只取说明第一行、且不列 direct 服务器: {section}"
        );
        assert!(
            !section.contains("hidden-one"),
            "hidden 服务器不列: {section}"
        );
        assert!(
            section.find("- alpha").unwrap() < section.find("- mixed").unwrap()
                && section.find("- mixed").unwrap() < section.find("- zeta").unwrap(),
            "按名字排序: {section}"
        );

        // 省略行：只有在预算放不下时才补
        assert!(omitted_line(0).is_none());
        assert!(omitted_line(1).unwrap().contains("1 more server;"));
        assert!(omitted_line(3).unwrap().contains("3 more servers;"));

        // 禁用（`disabled: true`）的条目在下游被 `enabled_servers` 滤掉，此处只校验单条摘要规则
        let long = "x".repeat(600);
        let section = render_servers_section(&[named(
            "wide",
            entry(Some(McpExposure::Codemode), Some(&long)),
        )])
        .unwrap();
        assert_eq!(
            section.matches('x').count(),
            MAX_SERVER_DESCRIPTION_CHARS - 1,
            "超长说明截断到单台上限（末尾一个字符换成省略号）: {section}"
        );
        assert!(section.contains('…'), "{section}");
    }

    /// 2.8：`/mcp enable|disable <server> [user|project]` 写覆盖条目（这里用 user 作用域，
    /// 免得切进程 cwd 干扰并行测试；项目层的「只写最小条目」由 config 层测试覆盖）。
    #[test]
    fn command_enable_disable_writes_scope_override() {
        let _lock = lock();
        let _dir = AgentDirGuard::temp();
        let _home = HomeGuard::temp();
        let paths = config::ConfigPaths::discover(Path::new("."));
        config::add_server(
            &paths,
            config::ConfigScope::User,
            "demo",
            ServerEntry {
                command: Some("echo".to_string()),
                ..Default::default()
            },
        )
        .unwrap();

        let mut st = App::new();
        let last = |st: &App| {
            st.system_messages
                .last()
                .map(|(_, s)| crate::modes::interactive::app::sys_spans_text(&s.spans))
                .unwrap_or_default()
        };

        assert!(!command_mcp(&mut st, "mcp disable demo user"));
        let disabled_msg = last(&st);
        let written = std::fs::read_to_string(&paths.app_global).unwrap();

        assert!(!command_mcp(&mut st, "mcp enable demo user"));
        let written_enabled = std::fs::read_to_string(&paths.app_global).unwrap();

        // 未配置的服务器：不写悬空覆盖（加载时会因缺 command/url 报错）
        assert!(!command_mcp(&mut st, "mcp disable nope user"));
        let not_configured_msg = last(&st);
        let written_after = std::fs::read_to_string(&paths.app_global).unwrap();

        // 未知作用域
        assert!(!command_mcp(&mut st, "mcp disable demo bogus"));
        let unknown_scope_msg = last(&st);

        while crate::core::extensions::take_pending_ui().is_some() {}

        assert!(
            disabled_msg.contains("disabled in user config"),
            "{disabled_msg}"
        );
        assert!(written.contains("\"disabled\": true"), "{written}");
        assert!(
            written_enabled.contains("\"disabled\": false"),
            "{written_enabled}"
        );
        assert!(
            not_configured_msg.contains("is not configured"),
            "{not_configured_msg}"
        );
        assert!(!written_after.contains("nope"), "{written_after}");
        assert!(
            unknown_scope_msg.contains("Unknown scope"),
            "{unknown_scope_msg}"
        );
    }

    #[test]
    fn command_usage_is_reported() {
        let _dir = AgentDirGuard::temp();
        let mut st = App::new();
        command_mcp(&mut st, "mcp bogus");
        let last = st
            .system_messages
            .last()
            .map(|(_, s)| crate::modes::interactive::app::sys_spans_text(&s.spans))
            .unwrap_or_default();
        assert!(last.contains("Unknown MCP subcommand"), "got: {last}");
    }

    #[test]
    fn status_and_list_handle_empty_config() {
        let _lock = lock();
        let _dir = AgentDirGuard::temp();
        let _home = HomeGuard::temp();
        assert!(render_status().contains("No MCP servers are configured"));
        assert!(render_list().contains("No MCP servers are configured"));
    }

    #[test]
    fn startup_message_reports_enabled_and_blocked_counts() {
        let _lock = lock();
        let _dir = AgentDirGuard::temp();
        let _home = HomeGuard::temp();
        let project = tempfile::tempdir().unwrap();
        let cwd = project.path();
        let paths = config::ConfigPaths::discover(cwd);
        config::add_server(
            &paths,
            config::ConfigScope::User,
            "global",
            ServerEntry {
                command: Some("echo".to_string()),
                ..Default::default()
            },
        )
        .unwrap();

        // 项目不受信任：项目级服务器被拦截，不计入 enabled。
        crate::core::project_trust::set_session_trust(cwd, false);
        std::fs::write(
            &paths.shared_project,
            r#"{"mcpServers":{"local":{"command":"echo"}}}"#,
        )
        .unwrap();

        let message = startup_message(cwd.to_str().unwrap()).unwrap();
        assert_eq!(
            message,
            "MCP: 1 servers enabled (1 blocked by project trust)"
        );
        crate::core::project_trust::clear_session_trust(cwd);
    }

    #[test]
    fn startup_message_is_silent_without_servers() {
        let _lock = lock();
        let _dir = AgentDirGuard::temp();
        let _home = HomeGuard::temp();
        let project = tempfile::tempdir().unwrap();
        let cwd = project.path();
        crate::core::project_trust::set_session_trust(cwd, true);
        assert!(startup_message(cwd.to_str().unwrap()).is_none());
        crate::core::project_trust::clear_session_trust(cwd);
    }

    #[test]
    fn tool_is_hidden_without_configured_servers() {
        let _lock = lock();
        let _dir = AgentDirGuard::temp();
        let _home = HomeGuard::temp();
        assert!(Mcp.tools().is_empty(), "无服务器时应不注入 mcp 工具");

        let cwd = std::env::current_dir().unwrap();
        let paths = config::ConfigPaths::discover(&cwd);
        config::add_server(
            &paths,
            config::ConfigScope::User,
            "demo",
            ServerEntry {
                command: Some("echo".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        let tools = Mcp.tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "mcp");
    }

    /// 扩展声明的 MCP 服务器（`pi.registerMcpServer()` 等价物）：进连接列表、
    /// 进 `/mcp` 列表并标出所属扩展；扩展注销后随之消失。
    #[test]
    fn extension_declared_servers_join_config_and_listing() {
        let _lock = lock();
        let _dir = AgentDirGuard::temp();
        let _home = HomeGuard::temp();
        let cwd = std::env::current_dir().unwrap();
        crate::core::extensions::register_extension(DeclaredServerProbe);
        assert!(crate::core::extensions::set_extension_enabled(
            PROBE_EXT, true
        ));

        let servers = server_entries(cwd.to_str().unwrap());
        let declared = servers.get("probe-srv").expect("声明的服务器进连接列表");
        assert_eq!(declared.command.as_deref(), Some("echo"));
        assert_eq!(Mcp.tools().len(), 1, "有（声明的）服务器时注入 mcp 工具");

        let list = render_list();
        assert!(list.contains("probe-srv"), "got: {list}");
        assert!(list.contains("from extension probe-ext"), "got: {list}");

        // 禁用扩展 → 服务器不再出现在配置里（会话作用域）
        assert!(crate::core::extensions::set_extension_enabled(
            PROBE_EXT, false
        ));
        assert!(!server_entries(cwd.to_str().unwrap()).contains_key("probe-srv"));
        assert!(!render_list().contains("probe-srv"), "禁用后不应列出");

        assert!(crate::core::extensions::unregister_extension(PROBE_EXT));
    }

    /// 声明 MCP 服务器的探针扩展（测试用）。
    struct DeclaredServerProbe;

    impl Extension for DeclaredServerProbe {
        /// 扩展名（与 `/extension` 面板、启停开关一致）
        fn name(&self) -> &str {
            PROBE_EXT
        }

        /// 探针不提供工具：只验证 MCP 服务器声明面。
        fn tools(&self) -> Vec<ExtensionTool> {
            Vec::new()
        }

        /// 供测试断言的声明：一个 stdio 服务器。
        fn mcp_servers(&self) -> Vec<crate::core::extensions::DeclaredMcpServer> {
            vec![(
                "probe-srv".to_string(),
                serde_json::json!({ "command": "echo", "args": ["hello"] }),
            )]
        }
    }

    /// 测试探针扩展名
    const PROBE_EXT: &str = "probe-ext";

    #[test]
    fn status_lists_configured_server() {
        let _dir = AgentDirGuard::temp();
        let _home = HomeGuard::temp();
        let cwd = std::env::current_dir().unwrap();
        let paths = config::ConfigPaths::discover(&cwd);
        config::add_server(
            &paths,
            config::ConfigScope::User,
            "demo",
            config::ServerEntry {
                command: Some("echo".to_string()),
                ..Default::default()
            },
        )
        .unwrap();

        let status = render_status();
        assert!(status.contains("demo"), "got: {status}");
        assert!(status.contains("stdio echo"), "got: {status}");

        let list = render_list();
        assert!(list.contains("demo"), "got: {list}");
        assert!(
            list.contains(&format!("{} global", crate::APP_NAME)),
            "got: {list}"
        );
    }
}
