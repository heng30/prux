//! MCP 配置发现、合并、校验与安全写入。
//!
//! 配置位置改为全局优先，后覆盖前：
//!
//! | # | 路径 | 作用域 |
//! |---|------|--------|
//! | 1 | `~/.config/mcp/mcp.json` | 共享全局 |
//! | 2 | `~/.agents/mcp.json` | agents 全局 |
//! | 3 | `~/.agents/mcp/mcp.json` | agents 嵌套全局 |
//! | 4 | `agent_dir()/extensions/mcp/mcp.json` | prux 全局 |
//! | 5 | `<cwd>/.mcp.json` | 项目（需信任） |
//! | 6 | `<cwd>/.prux/mcp.json` | 项目 prux（需信任） |
//!
//! 支持 `//` 与 `/* */` 注释、递归深度合并（后覆盖前）、原子 rename + 跨进程文件锁、
//! 文件权限 0o600。项目级配置仅在项目受信任时加载。

use super::error::{Context as _, Result, bail};
use crate::{
    APP_NAME, PROJECT_SCOPE_NAME,
    core::settings_manager::agent_dir,
    utils::{
        file_lock::FileLock,
        http::{is_https_with_path, is_loopback_url},
        paths::home_dir,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::OpenOptions,
    io::Write as _,
    path::{Path, PathBuf},
};
use strum_macros::{EnumString, IntoStaticStr};
use url::Url;

/// 指定 `agent_dir` 下的 MCP 数据目录（测试可注入根目录）。
/// MCP 数据目录相对 `agent_dir()` 的路径（存放 `mcp.json` / `cache.json` / `oauth.json`）。
pub fn data_dir_for(agent_dir: &Path) -> PathBuf {
    agent_dir.join("extensions/mcp")
}

/// 当前进程的 MCP 数据目录：`agent_dir()/extensions/mcp`。
pub fn data_dir() -> PathBuf {
    data_dir_for(&agent_dir())
}

/// prux 全局 MCP 配置：`agent_dir()/extensions/mcp/mcp.json`（user 作用域的写入目标）。
pub fn app_global_path(agent_dir: &Path) -> PathBuf {
    data_dir_for(agent_dir).join("mcp.json")
}

/// 元数据缓存：`agent_dir()/extensions/mcp/cache.json`。
pub fn cache_path() -> PathBuf {
    data_dir().join("cache.json")
}

/// OAuth 凭据：`agent_dir()/extensions/mcp/oauth.json`。
pub fn credentials_path() -> PathBuf {
    data_dir().join("oauth.json")
}

/// stdio 服务器 stderr 日志目录：`agent_dir()/extensions/mcp/logs`。
pub fn logs_dir() -> PathBuf {
    data_dir().join("logs")
}

/// 配置作用域（`prux mcp` 子命令的 `--user` / `--project`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigScope {
    /// 用户作用域：写入 [`ConfigPaths::app_global`]。
    User,
    /// 项目作用域：写入 [`ConfigPaths::shared_project`]（`<cwd>/.mcp.json`）。
    Project,
}

/// 6 个配置位置的绝对路径集合。
#[derive(Debug, Clone)]
pub struct ConfigPaths {
    /// `~/.config/mcp/mcp.json`：与他工具共享的全局配置。
    pub shared_global: PathBuf,
    /// `~/.agents/mcp.json`：agents 全局配置。
    pub agents_global: PathBuf,
    /// `~/.agents/mcp/mcp.json`：agents 嵌套全局配置。
    pub agents_nested_global: PathBuf,
    /// `agent_dir()/extensions/mcp/mcp.json`：prux 全局配置，也是 user 作用域的写入目标。
    pub app_global: PathBuf,
    /// `<cwd>/.mcp.json`：项目共享配置，也是 project 作用域的写入目标。
    pub shared_project: PathBuf,
    /// `<cwd>/.prux/mcp.json`：项目 prux 配置。
    pub app_project: PathBuf,
}

impl ConfigPaths {
    /// 按当前进程的 home / agent_dir / cwd 推导全部路径。
    pub fn discover(cwd: &Path) -> Self {
        Self::for_roots(&home_dir(), &agent_dir(), cwd)
    }

    /// 显式根目录版本（测试用，避免依赖全局状态）。
    pub fn for_roots(home: &Path, agent_dir: &Path, cwd: &Path) -> Self {
        Self {
            shared_global: home.join(".config/mcp/mcp.json"),
            agents_global: home.join(".agents/mcp.json"),
            agents_nested_global: home.join(".agents/mcp/mcp.json"),
            app_global: app_global_path(agent_dir),
            shared_project: cwd.join(".mcp.json"),
            app_project: cwd.join(format!("{PROJECT_SCOPE_NAME}/mcp.json")),
        }
    }

    /// 写入目标：user 写 prux 全局，project 写 `.mcp.json`。
    pub fn write_path(&self, scope: ConfigScope) -> &Path {
        match scope {
            ConfigScope::User => &self.app_global,
            ConfigScope::Project => &self.shared_project,
        }
    }

    /// 按优先级列出参与合并的配置来源；`trusted_project` 为 false 时不含两个项目级来源。
    fn read_sources(&self, trusted_project: bool) -> Vec<ConfigSource> {
        let mut sources = vec![
            ConfigSource::new("shared global", self.shared_global.clone(), false),
            ConfigSource::new("agents global", self.agents_global.clone(), false),
            ConfigSource::new(
                "agents nested global",
                self.agents_nested_global.clone(),
                false,
            ),
            ConfigSource::new(
                &format!("{APP_NAME} global"),
                self.app_global.clone(),
                false,
            ),
        ];

        if trusted_project {
            sources.push(ConfigSource::new(
                "project .mcp.json",
                self.shared_project.clone(),
                true,
            ));
            sources.push(ConfigSource::new(
                &format!("project {APP_NAME} MCP"),
                self.app_project.clone(),
                true,
            ));
        }
        sources
    }
}

/// 配置来源（诊断展示用）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigSource {
    /// 人类可读的来源名（如 `prux global`），由 [`crate::APP_NAME`] 拼出，用于诊断输出。
    pub label: String,
    /// 配置文件绝对路径。
    pub path: PathBuf,
    /// 是否为项目级来源（仅在项目受信任时参与加载）。
    pub project: bool,
}

impl ConfigSource {
    /// 构造一个来源标记（人类可读 label + 配置文件绝对路径 + 是否项目级）。
    fn new(label: &str, path: PathBuf, project: bool) -> Self {
        Self {
            label: label.to_string(),
            path,
            project,
        }
    }
}

/// MCP 工具的暴露方式。
///
/// 配置里的字符串形式为 kebab-case：JSON 侧由 serde 的 `rename_all` 解析，
/// 与字符串的互转由 `strum` 派生（大小写不敏感），见 [`McpExposure::as_str`] / [`McpExposure::parse`]。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, EnumString, IntoStaticStr,
)]
#[serde(rename_all = "kebab-case")]
#[strum(serialize_all = "kebab-case", ascii_case_insensitive)]
pub enum McpExposure {
    /// 模型直接可见，也可在 codemode 脚本里调（默认，保持既有行为）。
    #[default]
    Direct,
    /// 模型不可见，只在 `codemode` 脚本里可调。
    Codemode,
    /// 同 `codemode`，但不进 codemode 的工具清单；codemode 本就不列 MCP 工具，行为等同。
    CodemodeDeferred,
    /// 模型不可见，经 `tool_search` 激活后才可见。
    Deferred,
    /// 任何调用都不可见（保留配置但不暴露）。
    Hidden,
}

impl McpExposure {
    /// 配置值（`strum` 派生的 kebab-case 字符串），用于 `/mcp` 面板与 CLI 回显。
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// 解析配置字符串（去首尾空白、大小写不敏感）；无法识别时返回 `None`。
    pub fn parse(value: &str) -> Option<Self> {
        value.trim().parse().ok()
    }
}

/// 配置文件的顶层结构（`mcpServers` 映射 + 顶层开关）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct McpConfig {
    /// `mcpServers`：服务器名 → 条目。
    pub mcp_servers: BTreeMap<String, ServerEntry>,
    /// `codemode` 或 `codemode-deferred` 暴露的服务器接入时是否自动启用 codemode 扩展； None = 默认开启
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_enable_codemode: Option<bool>,
    /// 未识别的顶层字段，原样保留以便回写时不丢配置。
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// HTTP 认证方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuthMode {
    /// OAuth 2.1（authorization code + PKCE 或 client credentials）。
    OAuth,
    /// 静态 bearer token（来自 `bearerToken` / `bearerTokenEnv` / `auth.provider`）。
    Bearer,
}

/// `auth` 字段：`"oauth"` / `"bearer"` / `false`（显式禁用）或 `{"provider": "<name>"}`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AuthSetting {
    /// 显式指定认证方式（`"oauth"` / `"bearer"`）。
    Mode(AuthMode),
    /// 布尔形式：`false` 表示显式禁用认证；`true` 非法（校验时报错）。
    Disabled(bool),
    /// `{"provider": "<name>"}`：用某 provider 的当前登录 token 作 bearer。
    ///
    /// token 在**建立连接时**读取（`auth.json` 的 key，或该 provider 的 OAuth 凭据，过期则先刷新）；
    /// 连接建立后不再重读，持久连接的服务器要换 token 需 `/mcp reload`。
    Provider {
        /// provider 名（`/login` / `auth.json` 里的名字，如 `deepseek`）。
        provider: String,
    },
}

impl AuthSetting {
    /// 显式指定的认证方式；`Disabled`（`auth: false`）与 `Provider` 形式返回 None。
    pub fn mode(&self) -> Option<AuthMode> {
        match self {
            Self::Mode(mode) => Some(*mode),
            Self::Disabled(_) | Self::Provider { .. } => None,
        }
    }

    /// `{"provider": "<name>"}` 声明的 provider 名；其它形式返回 None。
    pub fn provider(&self) -> Option<&str> {
        match self {
            Self::Provider { provider } => Some(provider),
            _ => None,
        }
    }
}

/// OAuth 授权类型。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OAuthGrantType {
    /// 浏览器授权码 + PKCE（默认）。
    #[default]
    AuthorizationCode,
    /// 客户端凭据（无需浏览器，需 clientId + clientSecret）。
    ClientCredentials,
}

/// OAuth 客户端注册方式（`oauth.clientRegistration`）。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, EnumString, IntoStaticStr,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
pub enum ClientRegistration {
    /// 动态客户端注册（RFC 7591，默认）：授权时向服务器的注册端点要一个 client_id。
    #[default]
    Dynamic,
    /// Client ID Metadata Document（SEP-991）：client_id 直接是本方托管的 metadata 文档 URL，
    /// 服务端宣告 `client_id_metadata_document_supported` 时生效；需配 [`OAuthConfig::client_metadata_url`]。
    Cimd,
}

impl ClientRegistration {
    /// 配置值（`strum` 派生的 lowercase 字符串），用于 CLI 回显与错误提示。
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// 解析配置字符串（去首尾空白、大小写不敏感）；无法识别时返回 `None`。
    pub fn parse(value: &str) -> Option<Self> {
        value.trim().parse().ok()
    }
}

/// OAuth 配置。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct OAuthConfig {
    /// 授权类型。
    pub grant_type: OAuthGrantType,
    /// 客户端注册方式。
    pub client_registration: ClientRegistration,
    /// CIMD 用的 Client ID Metadata Document 地址（`clientRegistration = "cimd"` 时必填）。
    ///
    /// 该模式下它同时充当 client_id：必须是无凭据的 https URL、且路径非根。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_metadata_url: Option<String>,
    /// 授权服务器元数据文档地址：配了就不再走发现流程，直接拉这个文档当元数据
    /// （用于服务器宣告的 metadata 缺失或有误的场景）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_server_metadata_url: Option<String>,
    /// 预注册的 OAuth 客户端 ID（缺省时走动态客户端注册）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// OAuth 客户端密钥。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    /// 请求的 scope，空格分隔。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// 追加到授权 URL 的自定义查询参数（不可覆盖流程自有的参数）。
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub authorization_params: BTreeMap<String, String>,
    /// 回调地址，默认 `http://127.0.0.1:3118/callback`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redirect_uri: Option<String>,
    /// 动态客户端注册时上报的客户端名称。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_name: Option<String>,
    /// 动态客户端注册时上报的客户端主页。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_uri: Option<String>,
    /// 动态客户端注册时上报的 logo 地址。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logo_uri: Option<String>,
    /// 跳过 issuer 元数据校验（用于不严格实现规范的服务器）。
    pub skip_issuer_metadata_validation: bool,
    /// 未识别的 OAuth 字段，原样保留。
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// 工具名通配匹配：模式里的 `*` 匹配任意字符，其余字符按字面量
fn wildcard_matches(pattern: &str, tool_name: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    let mut rest = tool_name;
    for (index, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if index == 0 {
            let Some(after) = rest.strip_prefix(part) else {
                return false;
            };
            rest = after;
        } else if index == parts.len() - 1 {
            return rest.ends_with(part);
        } else if let Some(at) = rest.find(part) {
            rest = &rest[at + part.len()..];
        } else {
            return false;
        }
    }
    true
}

/// 单个 MCP 服务器条目。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ServerEntry {
    /// stdio 传输：要启动的可执行文件。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// stdio 传输：传给可执行文件的参数。
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// stdio 传输：追加到子进程的环境变量。
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// stdio 传输：子进程工作目录（支持 `~/` 展开）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Streamable HTTP 传输：服务器端点 URL。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// HTTP 传输：随每个请求发送的额外请求头。
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// 认证方式（显式 `auth` 字段）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthSetting>,
    /// bearer 认证：token 字面值（支持 `${ENV}` 展开）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bearer_token: Option<String>,
    /// bearer 认证：存放 token 的环境变量名。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bearer_token_env: Option<String>,
    /// OAuth 配置块。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oauth: Option<OAuthConfig>,
    /// 生命周期：`persistent` 表示不因空闲自动断开。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lifecycle: Option<String>,
    /// 空闲多少分钟后自动断开，默认 10。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_timeout: Option<u64>,
    /// 单次 RPC 超时（毫秒），默认 60_000。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_timeout_ms: Option<u64>,
    /// 工具白名单（非空时只暴露列出的工具）。
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub include_tools: Vec<String>,
    /// 工具黑名单（优先级高于白名单）。
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub exclude_tools: Vec<String>,
    /// 是否禁用该服务器（禁用的服务器不连接、不暴露工具）。
    pub disabled: bool,
    /// 人类可读的说明：进 `mcp` 工具的描述与 `/mcp` 列表，用于 tool search 排序与人工识别。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// 该服务器工具的默认暴露方式；None = `direct`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exposure: Option<McpExposure>,
    /// 单个工具的暴露方式覆盖（键为服务器提供的工具名，`*` 匹配任意字符）；
    /// 精确名优先，其次按声明顺序取第一个匹配的通配模式。
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub tool_exposure: BTreeMap<String, McpExposure>,
    /// 未识别的服务器字段，原样保留以便回写时不丢配置。
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl ServerEntry {
    /// 校验单条服务器配置（名称 + 传输 + 认证）。
    pub fn validate(&self, name: &str) -> Result<()> {
        validate_server_name(name)?;
        match (self.command.as_deref(), self.url.as_deref()) {
            (Some(command), None) if !command.trim().is_empty() => {}
            (None, Some(url)) => {
                let parsed = Url::parse(url)
                    .with_context(|| format!("MCP server `{name}` has an invalid URL"))?;
                if !matches!(parsed.scheme(), "http" | "https") {
                    bail!("MCP server `{name}` URL must use http or https");
                }
            }
            (Some(_), Some(_)) => {
                bail!("MCP server `{name}` must use either command or url, not both")
            }
            _ => bail!(
                "MCP server `{name}` must define command or url (a project-level entry without one only works as an override of a user-level server with the same name)"
            ),
        }

        if matches!(self.auth, Some(AuthSetting::Disabled(true))) {
            bail!("MCP server `{name}` uses invalid auth value true. Use false to disable auth")
        }

        if self.command.is_some()
            && (self.auth.is_some()
                || self.oauth.is_some()
                || self.bearer_token.is_some()
                || self.bearer_token_env.is_some())
        {
            bail!("MCP server `{name}` can use HTTP authentication only with url")
        }

        if let Some(oauth) = &self.oauth
            && oauth.grant_type == OAuthGrantType::ClientCredentials
            && (oauth.client_id.as_deref().unwrap_or("").is_empty()
                || oauth.client_secret.as_deref().unwrap_or("").is_empty())
        {
            bail!("MCP server `{name}` client_credentials needs clientId and clientSecret")
        }

        if let Some(oauth) = &self.oauth
            && oauth.client_registration == ClientRegistration::Cimd
        {
            let document = oauth.client_metadata_url.as_deref().unwrap_or("");
            if !is_https_with_path(document) {
                bail!(
                    "MCP server `{name}` oauth.clientRegistration={{cimd}} needs clientMetadataUrl to be an https URL with a non-root path"
                )
            }
        }

        if let Some(metadata) = &self
            .oauth
            .as_ref()
            .and_then(|o| o.auth_server_metadata_url.as_deref())
        {
            let parsed = Url::parse(metadata).with_context(|| {
                format!("MCP server `{name}` has an invalid oauth.authServerMetadataUrl")
            })?;
            if !matches!(parsed.scheme(), "http" | "https") {
                bail!("MCP server `{name}` oauth.authServerMetadataUrl must use http or https")
            }
        }

        // provider 形式的 bearer：token 会随每次 HTTP 请求发出去，非 loopback 端点必须是 https。
        if let Some(provider) = self.auth_provider() {
            if provider.trim().is_empty() {
                bail!("MCP server `{name}` auth.provider cannot be empty")
            }

            if let Some(url) = self.url.as_deref()
                && !is_loopback_url(url)
                && !url
                    .trim_start()
                    .to_ascii_lowercase()
                    .starts_with("https://")
            {
                bail!("MCP server `{name}` auth.provider requires https for non-loopback URLs")
            }
        }

        Ok(())
    }

    /// 推断认证方式（显式 `auth` 优先，其次 oauth / bearer token 字段）。
    /// `auth: {"provider": ...}` 归入 [`AuthMode::Bearer`]（同样是 `Authorization` 头）。
    pub fn auth_mode(&self) -> Option<AuthMode> {
        self.auth.as_ref().and_then(AuthSetting::mode).or_else(|| {
            if self.auth.as_ref().and_then(AuthSetting::provider).is_some() {
                return Some(AuthMode::Bearer);
            }

            self.oauth.as_ref().map(|_| AuthMode::OAuth).or_else(|| {
                (self.bearer_token.is_some() || self.bearer_token_env.is_some())
                    .then_some(AuthMode::Bearer)
            })
        })
    }

    /// `auth: {"provider": "<name>"}` 声明的 provider 名；token 由该 provider 的登录凭据提供。
    pub fn auth_provider(&self) -> Option<&str> {
        self.auth.as_ref().and_then(AuthSetting::provider)
    }

    /// 该服务器某个工具的暴露方式：`toolExposure` 精确名 → 首个匹配的通配模式 →
    /// 服务器 `exposure` → 默认 [`McpExposure::Direct`]
    pub fn exposure_for(&self, tool_name: &str) -> McpExposure {
        if let Some(exposure) = self.tool_exposure.get(tool_name) {
            return *exposure;
        }

        for (pattern, exposure) in &self.tool_exposure {
            if pattern.contains('*') && wildcard_matches(pattern, tool_name) {
                return *exposure;
            }
        }
        self.exposure.unwrap_or_default()
    }

    /// 该服务器是否声明了 `codemode` 系暴露（决定是否自动启用 codemode 扩展）。
    pub fn uses_codemode_exposure(&self) -> bool {
        if matches!(
            self.exposure,
            Some(McpExposure::Codemode | McpExposure::CodemodeDeferred)
        ) {
            return true;
        }

        self.tool_exposure.values().any(|exposure| {
            matches!(
                exposure,
                McpExposure::Codemode | McpExposure::CodemodeDeferred
            )
        })
    }

    /// 脱敏副本（展示 / 落盘前用）。
    pub fn redacted(&self) -> Self {
        let mut copy = self.clone();
        if copy.bearer_token.is_some() {
            copy.bearer_token = Some("[REDACTED]".to_string());
        }

        if let Some(oauth) = &mut copy.oauth
            && oauth.client_secret.is_some()
        {
            oauth.client_secret = Some("[REDACTED]".to_string());
        }

        for (name, value) in &mut copy.headers {
            if secret_name(name) || looks_secret(value) {
                *value = "[REDACTED]".to_string();
            }
        }

        for (name, value) in &mut copy.env {
            if secret_name(name) {
                *value = "[REDACTED]".to_string();
            }
        }
        copy
    }
}

/// 名字是否像密钥（含 authorization/cookie/key/token/secret/password 之一，大小写不敏感）。
fn secret_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    [
        "authorization",
        "cookie",
        "key",
        "token",
        "secret",
        "password",
    ]
    .iter()
    .any(|part| name.contains(part))
}

/// 值是否像凭据（以 `bearer ` 或 `basic ` 开头，大小写不敏感）。
fn looks_secret(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.starts_with("bearer ") || lower.starts_with("basic ")
}

/// 合并后的配置 + 各服务器来源。
#[derive(Debug, Clone)]
pub struct LoadedConfig {
    /// 6 个位置深度合并后的配置。
    pub config: McpConfig,
    /// 每个服务器名贡献过的配置来源（按加载顺序）。
    pub sources: BTreeMap<String, Vec<ConfigSource>>,
    /// 参与合并的路径集合，供写回使用。
    pub paths: ConfigPaths,
}

impl LoadedConfig {
    /// 未禁用（`disabled != true`）的服务器数量。
    pub fn enabled_server_count(&self) -> usize {
        self.config
            .mcp_servers
            .values()
            .filter(|server| !server.disabled)
            .count()
    }

    /// 某服务器贡献过的来源标签，形如 `label (path)`；无记录时为空。
    pub fn source_labels(&self, name: &str) -> Vec<String> {
        self.sources
            .get(name)
            .into_iter()
            .flatten()
            .map(|source| format!("{} ({})", source.label, source.path.display()))
            .collect()
    }
}

/// 加载：项目配置是否生效由调用方判定（`trusted_project`）。
pub fn load(cwd: &Path, trusted_project: bool) -> Result<LoadedConfig> {
    load_with_paths(ConfigPaths::discover(cwd), trusted_project)
}

/// 显式路径版本。
pub fn load_with_paths(paths: ConfigPaths, trusted_project: bool) -> Result<LoadedConfig> {
    let mut merged = Value::Object(Default::default());
    let mut sources: BTreeMap<String, Vec<ConfigSource>> = BTreeMap::new();

    for source in paths.read_sources(trusted_project) {
        let Some(value) = read_json_value(&source.path)? else {
            continue;
        };

        if let Some(servers) = value.get("mcpServers").and_then(Value::as_object) {
            for name in servers.keys() {
                sources
                    .entry(name.clone())
                    .or_default()
                    .push(source.clone());
            }
        }

        // `auth.provider` 让 MCP 请求借用某个 provider 的登录凭据：只允许出现在用户级配置里。
        // 项目文件（`.mcp.json` 常被提交进仓库）指定 token 来源，等于让仓库替用户决定把哪个
        // 凭据发给哪台服务器，故直接在加载时报错；扩展声明（来源不是项目级）不受此限。
        if source.project
            && let Some(servers) = value.get("mcpServers").and_then(Value::as_object)
        {
            for (name, entry) in servers {
                if entry
                    .get("auth")
                    .and_then(|auth| auth.get("provider"))
                    .is_some()
                {
                    bail!(
                        "MCP server `{name}` auth.provider is only allowed in user-level config or extension declarations"
                    )
                }
            }
        }

        deep_merge(&mut merged, value);
    }

    let config: McpConfig = serde_json::from_value(merged).context("invalid merged MCP config")?;
    for (name, server) in &config.mcp_servers {
        server.validate(name)?;
    }

    Ok(LoadedConfig {
        config,
        sources,
        paths,
    })
}

/// 只读取某个作用域的配置文件（不含合并）。
pub fn read_scope(paths: &ConfigPaths, scope: ConfigScope) -> Result<McpConfig> {
    match read_json_value(paths.write_path(scope))? {
        Some(value) => serde_json::from_value(value).context("invalid MCP config"),
        None => Ok(McpConfig::default()),
    }
}

/// 新增服务器；同名已存在时报错（提示改用 update）。
pub fn add_server(
    paths: &ConfigPaths,
    scope: ConfigScope,
    name: &str,
    server: ServerEntry,
) -> Result<()> {
    server.validate(name)?;

    let mut config = read_scope(paths, scope)?;
    if config.mcp_servers.contains_key(name) {
        bail!("MCP server `{name}` already exists in this scope. Use update")
    }

    config.mcp_servers.insert(name.to_string(), server);
    write_scope(paths, scope, &config)
}

/// 覆盖写入服务器（存在则替换）。
pub fn put_server(
    paths: &ConfigPaths,
    scope: ConfigScope,
    name: &str,
    server: ServerEntry,
) -> Result<()> {
    server.validate(name)?;
    let mut config = read_scope(paths, scope)?;
    config.mcp_servers.insert(name.to_string(), server);
    write_scope(paths, scope, &config)
}

/// 删除服务器，返回是否真的删掉了。
pub fn remove_server(paths: &ConfigPaths, scope: ConfigScope, name: &str) -> Result<bool> {
    let mut config = read_scope(paths, scope)?;
    let removed = config.mcp_servers.remove(name).is_some();
    if removed {
        write_scope(paths, scope, &config)?;
    }
    Ok(removed)
}

/// 启用 / 禁用服务器。
///
/// **只写最小覆盖条目**（`{"disabled": true}` 这种），不把 `inherited` 里借来的 command/url/凭据
/// 抄进目标作用域：项目层 `/.mcp.json` 很可能被提交进仓库，抄进去等于把用户层的 token 一起提交。
/// 条目能否拼出可用服务器，由加载时的合并后校验负责。
pub fn set_disabled(
    paths: &ConfigPaths,
    scope: ConfigScope,
    name: &str,
    disabled: bool,
) -> Result<()> {
    let mut config = read_scope(paths, scope)?;
    let server = config.mcp_servers.entry(name.to_string()).or_default();
    server.disabled = disabled;
    write_scope(paths, scope, &config)
}

/// 写入某个作用域的配置文件。
pub fn write_scope(paths: &ConfigPaths, scope: ConfigScope, config: &McpConfig) -> Result<()> {
    write_json_atomic(paths.write_path(scope), config)
}

/// 把扩展声明的 MCP 服务器合并进已加载配置
///
/// `declared` 是 `(所属扩展名, 服务器名, 条目 JSON)`，按声明顺序。规则：
/// - **`mcp.json` 同名条目优先**：已有文件来源时不覆盖，只把声明记成来源标注（供 `/mcp` 展示“被覆盖”）；
/// - 同批声明里重名、名字非法或条目校验不过时，跳过该声明并在返回值里报错；
/// - 声明进来源表时标为 `extension <扩展名> (<session>)`，与文件来源同形展示。
///
/// 返回每条未生效声明的错误说明（空 = 全部生效）。
pub fn apply_declared_servers(
    loaded: &mut LoadedConfig,
    declared: Vec<(String, String, Value)>,
) -> Vec<String> {
    let mut errors: Vec<String> = Vec::new();

    for (owner, name, entry) in declared {
        let label = format!("extension {owner}");
        let source = ConfigSource::new(&label, PathBuf::from("<session>"), false);

        if let Err(e) = validate_server_name(&name) {
            errors.push(format!("extension {owner}: server `{name}`: {e}"));
            continue;
        }

        let server: ServerEntry = match serde_json::from_value(entry) {
            Ok(server) => server,
            Err(e) => {
                errors.push(format!("extension {owner}: server `{name}`: {e}"));
                continue;
            }
        };

        if let Err(e) = server.validate(&name) {
            errors.push(format!("extension {owner}: server `{name}`: {e}"));
            continue;
        }

        // 文件里已有同名服务器：保留文件条目，只追加来源标注（展示覆盖关系）
        if loaded.config.mcp_servers.contains_key(&name) {
            loaded.sources.entry(name).or_default().push(source);
            continue;
        }

        loaded.sources.entry(name.clone()).or_default().push(source);
        loaded.config.mcp_servers.insert(name, server);
    }

    errors
}

/// 校验服务器名：仅 ASCII 字母数字与 `._-`。
pub fn validate_server_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "._-".contains(character))
    {
        bail!("MCP server name must use letters, numbers, dot, underscore, or hyphen")
    }
    Ok(())
}

/// 解析 `KEY=VALUE` 形式的重复参数（env / headers）。
pub fn parse_pairs(values: &[String], label: &str) -> Result<BTreeMap<String, String>> {
    let mut pairs = BTreeMap::new();
    for value in values {
        let Some((key, item)) = value.split_once('=') else {
            bail!("{label} must use KEY=VALUE: `{value}`")
        };

        if key.trim().is_empty() {
            bail!("{label} key cannot be empty")
        }

        pairs.insert(key.trim().to_string(), item.to_string());
    }
    Ok(pairs)
}

/// 读取并解析一个 JSON 配置文件（先剥掉 `//` 与 `/* */` 注释）。
/// 文件不存在时返回 `Ok(None)`，读取或解析失败则返回错误。
fn read_json_value(path: &Path) -> Result<Option<Value>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let cleaned = strip_json_comments(&text);
    let value = serde_json::from_str(&cleaned)
        .with_context(|| format!("parse MCP config {}", path.display()))?;
    Ok(Some(value))
}

/// 去掉 JSON 里的 `//` 与 `/* */` 注释（保留字符串字面量内的内容与换行）。
fn strip_json_comments(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;

    while let Some(character) = chars.next() {
        if in_string {
            output.push(character);

            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }

            continue;
        }

        if character == '"' {
            in_string = true;
            output.push(character);
            continue;
        }

        if character == '/' && chars.peek() == Some(&'/') {
            chars.next();
            for next in chars.by_ref() {
                if next == '\n' {
                    output.push('\n');
                    break;
                }
            }
            continue;
        }

        if character == '/' && chars.peek() == Some(&'*') {
            chars.next();
            let mut previous = '\0';
            for next in chars.by_ref() {
                if next == '\n' {
                    output.push('\n');
                }
                if previous == '*' && next == '/' {
                    break;
                }
                previous = next;
            }
            continue;
        }
        output.push(character);
    }
    output
}

/// 递归深度合并：两边都是对象则逐键合并，其余情况由 overlay 直接覆盖 base。
fn deep_merge(base: &mut Value, overlay: Value) {
    match (base, overlay) {
        (Value::Object(base), Value::Object(overlay)) => {
            for (key, value) in overlay {
                match base.get_mut(&key) {
                    Some(slot) => deep_merge(slot, value),
                    None => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (slot, value) => *slot = value,
    }
}

/// 原子写入配置：建父目录、加跨进程文件锁、写临时文件（0o600）后 rename 覆盖目标。
/// 失败时清理临时文件并把错误原样返回。
fn write_json_atomic(path: &Path, config: &McpConfig) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("MCP config path has no parent: {}", path.display()))?;
    std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;

    // 跨进程排他锁（sidecar 锁文件），避免并发读-改-写丢更新。
    let _lock = FileLock::acquire(path)?;

    let temporary = path.with_extension(format!("json.tmp.{}", std::process::id()));
    let result = (|| -> Result<()> {
        let mut file = secure_create(&temporary)?;
        serde_json::to_writer_pretty(&mut file, config)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        std::fs::rename(&temporary, path).with_context(|| format!("replace {}", path.display()))?;
        Ok(())
    })();

    _ = std::fs::remove_file(&temporary);
    result
}

/// 以 0o600 权限（unix）创建/截断文件并返回可写句柄。
pub fn secure_create(path: &Path) -> Result<std::fs::File> {
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }

    options
        .open(path)
        .with_context(|| format!("create {}", path.display()))
}

/// 全部（受信任范围内）配置源路径。
pub fn all_source_paths(paths: &ConfigPaths, trusted_project: bool) -> BTreeSet<PathBuf> {
    paths
        .read_sources(trusted_project)
        .into_iter()
        .map(|source| source.path)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn paths(home: &Path, project: &Path, agent_dir: &Path) -> ConfigPaths {
        ConfigPaths::for_roots(home, agent_dir, project)
    }

    fn stdio(command: &str) -> ServerEntry {
        ServerEntry {
            command: Some(command.to_string()),
            ..Default::default()
        }
    }

    /// 暴露方式：精确名 > 首个匹配的通配模式 > 服务器 exposure > 默认 direct。
    #[test]
    fn tool_exposure_resolution_prefers_exact_then_wildcard() {
        let mut entry = stdio("cmd");
        assert_eq!(entry.exposure_for("anything"), McpExposure::Direct);

        entry.exposure = Some(McpExposure::Codemode);
        assert_eq!(entry.exposure_for("anything"), McpExposure::Codemode);

        entry
            .tool_exposure
            .insert("read_*".to_string(), McpExposure::Hidden);
        entry
            .tool_exposure
            .insert("read_file".to_string(), McpExposure::Direct);
        assert_eq!(entry.exposure_for("read_file"), McpExposure::Direct);
        assert_eq!(entry.exposure_for("read_dir"), McpExposure::Hidden);
        assert_eq!(entry.exposure_for("write_file"), McpExposure::Codemode);
    }

    /// `codemode` 系暴露（含逐工具覆盖）能被识别，用于 `autoEnableCodemode`。
    #[test]
    fn codemode_exposure_is_detected() {
        let mut entry = stdio("cmd");
        assert!(!entry.uses_codemode_exposure());

        entry
            .tool_exposure
            .insert("search".to_string(), McpExposure::CodemodeDeferred);
        assert!(entry.uses_codemode_exposure());

        entry.tool_exposure.clear();
        entry.exposure = Some(McpExposure::Deferred);
        assert!(!entry.uses_codemode_exposure());
    }

    /// strum 派生的双向转换：`as_str` 是规范 kebab-case 名，`parse` 去空白且大小写不敏感。
    #[test]
    fn exposure_strum_conversions() {
        assert_eq!(McpExposure::CodemodeDeferred.as_str(), "codemode-deferred");
        assert_eq!(
            McpExposure::parse("codemode-deferred"),
            Some(McpExposure::CodemodeDeferred)
        );
        assert_eq!(McpExposure::parse(" HIDDEN "), Some(McpExposure::Hidden));
        assert_eq!(McpExposure::parse("bogus"), None);
        assert_eq!(McpExposure::default(), McpExposure::Direct);
    }

    /// strum 派生的双向转换：`as_str` 是规范 lowercase 名，`parse` 去空白且大小写不敏感。
    #[test]
    fn client_registration_strum_conversions() {
        assert_eq!(ClientRegistration::Dynamic.as_str(), "dynamic");
        assert_eq!(ClientRegistration::Cimd.as_str(), "cimd");
        assert_eq!(
            ClientRegistration::parse("cimd"),
            Some(ClientRegistration::Cimd)
        );
        assert_eq!(
            ClientRegistration::parse(" DYNAMIC "),
            Some(ClientRegistration::Dynamic)
        );
        assert_eq!(ClientRegistration::parse("bogus"), None);
        assert_eq!(ClientRegistration::default(), ClientRegistration::Dynamic);
    }

    /// `mcp.json` 的 `exposure` / `toolExposure` / 顶层 `autoEnableCodemode` 可解析；
    /// 非法 exposure 取值报错。
    #[test]
    fn exposure_fields_parse_from_json() {
        let config: McpConfig = serde_json::from_value(json!({
            "autoEnableCodemode": false,
            "mcpServers": {
                "docs": {
                    "url": "https://example.com/mcp",
                    "exposure": "codemode-deferred",
                    "toolExposure": { "read_*": "hidden", "search": "direct" }
                }
            }
        }))
        .unwrap();
        assert_eq!(config.auto_enable_codemode, Some(false));
        let entry = config.mcp_servers.get("docs").unwrap();
        assert_eq!(entry.exposure, Some(McpExposure::CodemodeDeferred));
        assert_eq!(entry.exposure_for("read_file"), McpExposure::Hidden);
        assert_eq!(entry.exposure_for("search"), McpExposure::Direct);

        let bad = serde_json::from_value::<McpConfig>(json!({
            "mcpServers": { "docs": { "url": "https://example.com/mcp", "exposure": "bogus" } }
        }));
        assert!(bad.is_err(), "非法 exposure 应报错");
    }

    #[test]
    fn project_config_overrides_global_fields() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let project = temp.path().join("project");
        let agent_dir = home.join(".prux");
        let paths = paths(&home, &project, &agent_dir);

        let mut global = McpConfig::default();
        let mut entry = stdio("global-command");
        entry.args = vec!["one".to_string()];
        global.mcp_servers.insert("demo".to_string(), entry);
        write_json_atomic(&paths.app_global, &global).unwrap();

        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            &paths.shared_project,
            r#"{"mcpServers":{"demo":{"command":"project-command"}}}"#,
        )
        .unwrap();

        let loaded = load_with_paths(paths, true).unwrap();
        let demo = &loaded.config.mcp_servers["demo"];
        assert_eq!(demo.command.as_deref(), Some("project-command"));
        assert_eq!(demo.args, ["one"]);
    }

    #[test]
    fn untrusted_project_config_is_not_loaded() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let project = temp.path().join("project");
        let agent_dir = home.join(".prux");
        let paths = paths(&home, &project, &agent_dir);
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            &paths.shared_project,
            r#"{"mcpServers":{"project":{"command":"danger"}}}"#,
        )
        .unwrap();
        let loaded = load_with_paths(paths, false).unwrap();
        assert!(loaded.config.mcp_servers.is_empty());
    }

    #[test]
    fn crud_is_scoped_and_keeps_json_comments_readable() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let project = temp.path().join("project");
        let agent_dir = home.join(".prux");
        let paths = paths(&home, &project, &agent_dir);
        std::fs::create_dir_all(paths.app_global.parent().unwrap()).unwrap();
        std::fs::write(
            &paths.app_global,
            "{\n// shared comment\n\"mcpServers\": {}\n}",
        )
        .unwrap();
        add_server(&paths, ConfigScope::User, "demo", stdio("tool")).unwrap();
        let loaded = load_with_paths(paths.clone(), false).unwrap();
        assert_eq!(
            loaded.config.mcp_servers["demo"].command.as_deref(),
            Some("tool")
        );
        assert!(remove_server(&paths, ConfigScope::User, "demo").unwrap());
        assert!(!remove_server(&paths, ConfigScope::User, "demo").unwrap());
    }

    #[test]
    fn rejects_ambiguous_transport_and_redacts_secrets() {
        let mut entry = stdio("tool");
        entry.url = Some("https://example.com/mcp".to_string());
        assert!(entry.validate("demo").is_err());

        let entry = ServerEntry {
            url: Some("https://example.com/mcp".to_string()),
            bearer_token: Some("secret".to_string()),
            env: BTreeMap::from([
                ("API_TOKEN".to_string(), "environment-secret".to_string()),
                ("MODE".to_string(), "safe".to_string()),
            ]),
            headers: BTreeMap::from([
                ("Authorization".to_string(), "opaque-secret".to_string()),
                ("X-Mode".to_string(), "safe".to_string()),
            ]),
            oauth: Some(OAuthConfig {
                client_secret: Some("client-secret".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let redacted = entry.redacted();
        assert_eq!(redacted.bearer_token.as_deref(), Some("[REDACTED]"));
        assert_eq!(redacted.env["API_TOKEN"], "[REDACTED]");
        assert_eq!(redacted.env["MODE"], "safe");
        assert_eq!(redacted.headers["Authorization"], "[REDACTED]");
        assert_eq!(redacted.headers["X-Mode"], "safe");
        assert_eq!(
            redacted.oauth.unwrap().client_secret.as_deref(),
            Some("[REDACTED]")
        );
    }

    #[cfg(unix)]
    #[test]
    fn owned_config_uses_private_file_mode() {
        use std::os::unix::fs::PermissionsExt as _;
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let project = temp.path().join("project");
        let agent_dir = home.join(".prux");
        let paths = paths(&home, &project, &agent_dir);
        add_server(&paths, ConfigScope::User, "demo", stdio("tool")).unwrap();
        let mode = std::fs::metadata(paths.app_global)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn strips_comments_but_preserves_strings() {
        let input = r#"{
  // line comment
  "url": "http://example.com/*not-a-comment*/",
  /* block
     comment */
  "n": 1
}"#;
        let value: Value = serde_json::from_str(&strip_json_comments(input)).unwrap();
        assert_eq!(value["url"], "http://example.com/*not-a-comment*/");
        assert_eq!(value["n"], 1);
    }
    /// 2.8：项目层只写 `disabled` / `exposure` 的**局部覆盖**条目，合并后仍是可用服务器。
    #[test]
    fn project_partial_override_merges_into_global_server() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let project = temp.path().join("project");
        let paths = paths(&home, &project, &home.join(".prux"));

        let mut global = McpConfig::default();
        global.mcp_servers.insert(
            "demo".to_string(),
            ServerEntry {
                url: Some("https://example.com/mcp".to_string()),
                ..Default::default()
            },
        );
        write_json_atomic(&paths.app_global, &global).unwrap();

        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            &paths.shared_project,
            r#"{"mcpServers":{"demo":{"disabled":true,"exposure":"codemode"}}}"#,
        )
        .unwrap();

        let loaded = load_with_paths(paths, true).unwrap();
        let demo = &loaded.config.mcp_servers["demo"];
        assert!(demo.disabled, "项目层 disabled 应覆盖");
        assert_eq!(demo.exposure, Some(McpExposure::Codemode));
        assert_eq!(
            demo.url.as_deref(),
            Some("https://example.com/mcp"),
            "url 仍来自用户层"
        );
    }

    /// 2.8：`set_disabled` 只写最小覆盖条目——项目文件里不该出现用户层的 command/url/凭据。
    #[test]
    fn set_disabled_writes_minimal_override() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let project = temp.path().join("project");
        let paths = paths(&home, &project, &home.join(".prux"));

        let mut global = McpConfig::default();
        global.mcp_servers.insert(
            "demo".to_string(),
            ServerEntry {
                url: Some("https://example.com/mcp".to_string()),
                bearer_token: Some("super-secret".to_string()),
                ..Default::default()
            },
        );
        write_json_atomic(&paths.app_global, &global).unwrap();
        std::fs::create_dir_all(&project).unwrap();

        set_disabled(&paths, ConfigScope::Project, "demo", true).unwrap();

        let written = std::fs::read_to_string(&paths.shared_project).unwrap();
        assert!(written.contains("\"disabled\": true"), "{written}");
        assert!(
            !written.contains("super-secret"),
            "凭据不得抄进项目文件: {written}"
        );
        assert!(
            !written.contains("example.com"),
            "url 不得抄进项目文件: {written}"
        );

        let loaded = load_with_paths(paths, true).unwrap();
        let demo = &loaded.config.mcp_servers["demo"];
        assert!(demo.disabled);
        assert_eq!(demo.bearer_token.as_deref(), Some("super-secret"));
    }

    /// 2.9 / 2.10：CIMD 必须有 https 的 metadata 文档地址；authServerMetadataUrl 只收 http(s)。
    #[test]
    fn oauth_client_registration_and_metadata_urls_are_validated() {
        let mut entry = ServerEntry {
            url: Some("https://example.com/mcp".to_string()),
            ..Default::default()
        };

        entry.oauth = Some(OAuthConfig {
            client_registration: ClientRegistration::Cimd,
            ..Default::default()
        });
        assert!(
            entry.validate("demo").is_err(),
            "cimd 缺 clientMetadataUrl 应报错"
        );

        entry.oauth.as_mut().unwrap().client_metadata_url =
            Some("http://example.com/client-metadata.json".to_string());
        assert!(entry.validate("demo").is_err(), "cimd 必须是 https");

        entry.oauth.as_mut().unwrap().client_metadata_url =
            Some("https://example.com/client-metadata.json".to_string());
        entry.validate("demo").unwrap();

        // dynamic 注册不需要该字段
        entry.oauth.as_mut().unwrap().client_registration = ClientRegistration::Dynamic;
        entry.oauth.as_mut().unwrap().client_metadata_url = None;
        entry.validate("demo").unwrap();

        entry.oauth.as_mut().unwrap().auth_server_metadata_url =
            Some("ftp://example.com/metadata".to_string());
        assert!(entry.validate("demo").is_err());
        entry.oauth.as_mut().unwrap().auth_server_metadata_url =
            Some("https://example.com/.well-known/oauth-authorization-server".to_string());
        entry.validate("demo").unwrap();
    }

    /// 2.11：`auth.provider` 形式归入 bearer，非 loopback 端点必须 https，provider 名不得为空。
    #[test]
    fn provider_auth_requires_https_outside_loopback() {
        let provider = |name: &str| AuthSetting::Provider {
            provider: name.to_string(),
        };
        let entry = |url: &str, name: &str| ServerEntry {
            url: Some(url.to_string()),
            auth: Some(provider(name)),
            ..Default::default()
        };

        let mut server = entry("http://example.com/mcp", "deepseek");
        assert!(
            server.validate("demo").is_err(),
            "非 loopback 明文 http 应报错"
        );
        assert_eq!(server.auth_mode(), Some(AuthMode::Bearer));
        assert_eq!(server.auth_provider(), Some("deepseek"));

        server.url = Some("http://127.0.0.1:8080/mcp".to_string());
        server.validate("demo").unwrap();
        server.url = Some("http://[::1]:8080/mcp".to_string());
        server.validate("demo").unwrap();
        server.url = Some("https://example.com/mcp".to_string());
        server.validate("demo").unwrap();

        let empty = entry("https://example.com/mcp", "  ");
        assert!(empty.validate("demo").is_err(), "空 provider 应报错");
    }

    /// 2.11：`auth.provider` 只允许出现在用户级配置（或扩展声明）里，项目文件里出现即报错。
    #[test]
    fn project_config_rejects_auth_provider() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let project = temp.path().join("project");
        let paths = paths(&home, &project, &home.join(".prux"));
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            &paths.shared_project,
            r#"{"mcpServers":{"demo":{"url":"https://example.com/mcp","auth":{"provider":"deepseek"}}}}"#,
        )
        .unwrap();

        let error = load_with_paths(paths, true).unwrap_err().to_string();
        assert!(error.contains("auth.provider"), "{error}");
    }

    /// 2.12：`description` 是普通展示字段，不参与脱敏。
    #[test]
    fn description_survives_redaction() {
        let entry = ServerEntry {
            url: Some("https://example.com/mcp".to_string()),
            description: Some("Issue tracker tools".to_string()),
            ..Default::default()
        };
        assert_eq!(
            entry.redacted().description.as_deref(),
            Some("Issue tracker tools")
        );
    }
}

#[cfg(test)]
mod declared_tests {
    use super::*;
    use serde_json::json;

    /// 构造一份"只有文件来源"的加载结果（模拟 mcp.json 已有 `file-srv`）。
    fn loaded_with_file_server() -> LoadedConfig {
        let mut config = McpConfig::default();
        config.mcp_servers.insert(
            "file-srv".to_string(),
            ServerEntry {
                command: Some("npx".to_string()),
                args: vec!["file".to_string()],
                ..Default::default()
            },
        );
        let mut sources = BTreeMap::new();
        sources.insert(
            "file-srv".to_string(),
            vec![ConfigSource::new(
                "global",
                PathBuf::from("/tmp/mcp.json"),
                false,
            )],
        );
        LoadedConfig {
            config,
            sources,
            paths: ConfigPaths::discover(Path::new(".")),
        }
    }

    /// 声明生效：条目进配置、来源标为所属扩展；mcp.json 同名条目优先并保留两份来源。
    #[test]
    fn declared_servers_merge_and_files_win() {
        let mut loaded = loaded_with_file_server();
        let errors = apply_declared_servers(
            &mut loaded,
            vec![
                (
                    "probe".to_string(),
                    "ext-srv".to_string(),
                    json!({ "url": "https://example.com/mcp", "exposure": "codemode" }),
                ),
                (
                    "probe".to_string(),
                    "file-srv".to_string(),
                    json!({ "command": "should-not-win" }),
                ),
            ],
        );
        assert!(errors.is_empty(), "{errors:?}");

        let ext = loaded.config.mcp_servers.get("ext-srv").expect("声明生效");
        assert_eq!(ext.url.as_deref(), Some("https://example.com/mcp"));
        assert_eq!(ext.exposure, Some(McpExposure::Codemode));
        let labels = loaded.source_labels("ext-srv");
        assert_eq!(labels.len(), 1, "{labels:?}");
        assert!(labels[0].contains("extension probe"), "{labels:?}");

        // 文件条目优先：命令仍是文件里的
        let file_srv = loaded.config.mcp_servers.get("file-srv").unwrap();
        assert_eq!(file_srv.command.as_deref(), Some("npx"));
        let labels = loaded.source_labels("file-srv");
        assert_eq!(
            labels.len(),
            2,
            "文件来源 + 被覆盖的扩展声明都记录: {labels:?}"
        );
        assert!(
            labels.iter().any(|l| l.contains("extension probe")),
            "{labels:?}"
        );
    }

    /// 非法名字 / 校验不过的条目不生效并报错（其它声明不受影响）。
    #[test]
    fn invalid_declarations_are_reported_and_skipped() {
        let mut loaded = loaded_with_file_server();
        let errors = apply_declared_servers(
            &mut loaded,
            vec![
                (
                    "probe".to_string(),
                    "bad name".to_string(),
                    json!({ "command": "x" }),
                ),
                ("probe".to_string(), "empty".to_string(), json!({})),
                (
                    "other".to_string(),
                    "ok-srv".to_string(),
                    json!({ "command": "x" }),
                ),
            ],
        );

        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(errors[0].contains("bad name"), "{errors:?}");
        assert!(errors[1].contains("empty"), "{errors:?}");
        assert!(!loaded.config.mcp_servers.contains_key("bad name"));
        assert!(!loaded.config.mcp_servers.contains_key("empty"));
        assert!(loaded.config.mcp_servers.contains_key("ok-srv"));
    }

    /// 同批声明重名时后者被丢弃（由 `declared_mcp_servers` 收集层保证，这里锁住工具函数的前置假设）。
    #[test]
    fn first_declaration_wins_for_duplicates_at_call_site() {
        let mut loaded = loaded_with_file_server();
        let errors = apply_declared_servers(
            &mut loaded,
            vec![(
                "probe".to_string(),
                "dup".to_string(),
                json!({ "command": "first" }),
            )],
        );
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(
            loaded
                .config
                .mcp_servers
                .get("dup")
                .unwrap()
                .command
                .as_deref(),
            Some("first")
        );
    }
}
