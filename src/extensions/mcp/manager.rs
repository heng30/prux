//! MCP 服务器生命周期与元数据缓存。
//!
//! 懒连接、stdio / Streamable HTTP 传输、空闲断开、每 RPC 超时 + 取消、
//! tools/resources/prompts 元数据缓存（按配置指纹失效）。
//! 缓存位于 `agent_dir()/extensions/mcp/cache.json`，
//! OAuth 凭据位于 `agent_dir()/extensions/mcp/oauth.json`。

use crate::{
    core::auth,
    extensions::mcp::{
        config::{self, AuthMode, LoadedConfig, ServerEntry, cache_path, credentials_path},
        ensure_tls_crypto_provider,
        error::{Context as _, McpError, Result, bail},
        oauth,
    },
    utils::{http as http_util, paths::expand_home},
};
use futures_util::stream::BoxStream;
use http::{HeaderName, HeaderValue};
use rmcp::{
    RoleClient, ServiceExt as _,
    model::{
        CallToolRequestParams, ClientInfo, ClientJsonRpcMessage, GetPromptRequestParams,
        JsonObject, ReadResourceRequestParams,
    },
    service::RunningService,
    transport::{
        StreamableHttpClientTransport, TokioChildProcess,
        auth::AuthClient,
        streamable_http_client::{
            StreamableHttpClient, StreamableHttpClientTransportConfig, StreamableHttpError,
            StreamableHttpPostResponse,
        },
    },
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use sse_stream::{Error as SseError, Sse};
use std::{
    collections::{BTreeMap, HashMap},
    io::Write as _,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{process::Command, sync::Mutex};
use tokio_util::sync::CancellationToken;

/// rmcp 客户端服务类型别名。
type Client = RunningService<RoleClient, ClientInfo>;

/// 缓存文件格式版本；不匹配时整份缓存作废重读。
const CACHE_VERSION: u32 = 1;

/// 未配置 `requestTimeoutMs` 时的单次 RPC 默认超时。
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// 单个 stderr 日志文件的字节上限；超限时下次连接清空重写。
const STDERR_LOG_LIMIT_BYTES: u64 = 1024 * 1024;

/// 缓存的工具元数据。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CachedTool {
    /// 所属服务器名。
    pub server: String,
    /// 工具名（调用时使用）。
    pub name: String,
    /// 可选的展示标题。
    pub title: Option<String>,
    /// 工具描述（`search` / `describe` 的匹配文本）。
    pub description: Option<String>,
    /// 输入参数的 JSON Schema。
    pub input_schema: Value,
}

/// 缓存的资源元数据。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CachedResource {
    /// 所属服务器名。
    pub server: String,
    /// 资源 URI（读取时使用）。
    pub uri: String,
    /// 资源名。
    pub name: String,
    /// 资源描述。
    pub description: Option<String>,
    /// MIME 类型（若有）。
    pub mime_type: Option<String>,
}

/// 缓存的 prompt 元数据。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CachedPrompt {
    /// 所属服务器名。
    pub server: String,
    /// prompt 名（获取时使用）。
    pub name: String,
    /// 可选的展示标题。
    pub title: Option<String>,
    /// prompt 描述。
    pub description: Option<String>,
    /// 参数定义的 JSON 表示。
    pub arguments: Value,
}

/// 服务器运行状态。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ServerState {
    /// 配置中被禁用，不参与连接。
    Disabled,
    /// 未连接且无缓存。
    NotConnected,
    /// 未连接但有缓存元数据。
    Cached,
    /// 已建立活动连接。
    Connected,
    /// 上次连接失败（见 [`ServerStatus::error`]）。
    Failed,
}

/// 单服务器状态快照（`mcp status` 用）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerStatus {
    /// 服务器名。
    pub name: String,
    /// 当前运行状态。
    pub state: ServerState,
    /// 缓存的工具数。
    pub tools: usize,
    /// 缓存的资源数。
    pub resources: usize,
    /// 缓存的 prompt 数。
    pub prompts: usize,
    /// 上次连接失败的错误信息。
    pub error: Option<String>,
}

/// 落盘的 MCP 元数据缓存文件，记录版本与各服务器已列出的能力。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct CacheFile {
    /// 写入时的缓存格式版本。
    version: u32,
    /// 服务器名 → 该服务器的元数据缓存。
    servers: BTreeMap<String, ServerCache>,
}

/// 单个服务器的元数据缓存。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct ServerCache {
    /// 生成缓存时的配置指纹；与当前配置不一致则丢弃。
    fingerprint: String,
    /// 已列出的工具。
    tools: Vec<CachedTool>,
    /// 已列出的资源。
    resources: Vec<CachedResource>,
    /// 已列出的 prompt。
    prompts: Vec<CachedPrompt>,
}

/// 单服务器的进程内运行态。
#[derive(Default)]
struct Runtime {
    /// 活动连接（`None` 表示未连接）。
    client: Option<Client>,
    /// 最近一次使用时刻，用于空闲断开判定。
    last_used: Option<Instant>,
    /// 最近一次连接失败的错误信息。
    error: Option<String>,
}

impl Runtime {
    /// 建一个未连接、无错误的默认运行态。
    fn new() -> Self {
        Self::default()
    }
}

/// 管理器的共享内部状态（`Arc` 包裹，便于克隆与后台任务持有）。
struct Inner {
    /// 合并后的配置。
    config: LoadedConfig,
    /// 每个服务器的运行态。
    runtimes: BTreeMap<String, Arc<Mutex<Runtime>>>,
    /// 内存中的元数据缓存（落盘到 [`Self::cache_path`]）。
    cache: Mutex<CacheFile>,
    /// 元数据缓存文件路径。
    cache_path: PathBuf,
    /// stdio 服务器 stderr 日志目录。
    log_dir: PathBuf,
    /// OAuth 凭据文件路径（`None` = 用默认位置）。
    credential_path: Option<PathBuf>,
}

/// MCP 管理器：持有合并配置、各服务器运行态与元数据缓存。
#[derive(Clone)]
pub struct McpManager {
    /// 共享内部状态。
    inner: Arc<Inner>,
}

impl McpManager {
    /// 默认位置：缓存与凭据都在 `agent_dir()` 下。
    pub fn new(config: LoadedConfig) -> Result<Self> {
        Ok(Self::with_paths(
            config,
            cache_path(),
            Some(credentials_path()),
        ))
    }

    /// 显式路径版本。
    pub fn with_paths(
        config: LoadedConfig,
        cache_path: PathBuf,
        credential_path: Option<PathBuf>,
    ) -> Self {
        let runtimes = config
            .config
            .mcp_servers
            .keys()
            .map(|name| (name.clone(), Arc::new(Mutex::new(Runtime::new()))))
            .collect();

        let mut cache = load_cache(&cache_path).unwrap_or_default();

        // 指纹不匹配（配置变了）的缓存直接丢弃。
        cache.servers.retain(|name, entry| {
            config
                .config
                .mcp_servers
                .get(name)
                .is_some_and(|server| entry.fingerprint == fingerprint(server))
        });

        Self {
            inner: Arc::new(Inner {
                config,
                runtimes,
                cache: Mutex::new(cache),
                log_dir: log_dir_for(&cache_path),
                cache_path,
                credential_path,
            }),
        }
    }

    /// 返回合并后的 MCP 配置引用。
    pub fn config(&self) -> &LoadedConfig {
        &self.inner.config
    }

    /// 每服务器状态（不触发连接）。
    pub async fn status(&self) -> Vec<ServerStatus> {
        let cache = self.inner.cache.lock().await.clone();
        let mut statuses = Vec::new();

        for (name, server) in &self.inner.config.config.mcp_servers {
            let guard = match self.inner.runtimes.get(name) {
                Some(runtime) => Some(runtime.lock().await),
                None => None,
            };

            let runtime = guard.as_deref();
            let cached = cache.servers.get(name);
            let state = if server.disabled {
                ServerState::Disabled
            } else if runtime.is_some_and(|runtime| runtime.client.is_some()) {
                ServerState::Connected
            } else if runtime.is_some_and(|runtime| runtime.error.is_some()) {
                ServerState::Failed
            } else if cached.is_some() {
                ServerState::Cached
            } else {
                ServerState::NotConnected
            };

            statuses.push(ServerStatus {
                name: name.clone(),
                state,
                tools: cached.map_or(0, |item| item.tools.len()),
                resources: cached.map_or(0, |item| item.resources.len()),
                prompts: cached.map_or(0, |item| item.prompts.len()),
                error: runtime.and_then(|runtime| runtime.error.clone()),
            });
        }

        statuses
    }

    /// 全部已缓存工具（不触发连接）。
    pub async fn cached_tools(&self) -> Vec<CachedTool> {
        self.inner
            .cache
            .lock()
            .await
            .servers
            .values()
            .flat_map(|server| server.tools.clone())
            .collect()
    }

    /// 列出工具（`None` = 所有非禁用服务器）。
    pub async fn list_tools(
        &self,
        server_name: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<Vec<CachedTool>> {
        let names = self.server_names(server_name)?;
        let mut tools = Vec::new();
        for name in names {
            tools.extend(self.refresh_tools(&name, cancel).await?);
        }
        Ok(tools)
    }

    /// 多词模糊搜索工具名 / 标题 / 描述 / 服务器名。
    pub async fn search_tools(
        &self,
        query: &str,
        server_name: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<Vec<CachedTool>> {
        let mut tools = Vec::new();
        let mut missing = Vec::new();
        let names = self.server_names(server_name)?;

        {
            let cache = self.inner.cache.lock().await;
            for name in names {
                if let Some(server) = cache.servers.get(&name) {
                    tools.extend(server.tools.clone());
                } else {
                    missing.push(name);
                }
            }
        }

        for name in missing {
            tools.extend(self.refresh_tools(&name, cancel).await?);
        }

        let terms: Vec<String> = query
            .split_whitespace()
            .map(|term| term.to_ascii_lowercase())
            .collect();

        tools.retain(|tool| {
            let haystack = format!(
                "{} {} {} {}",
                tool.server,
                tool.name,
                tool.title.as_deref().unwrap_or(""),
                tool.description.as_deref().unwrap_or("")
            )
            .to_ascii_lowercase();
            terms.iter().all(|term| haystack.contains(term))
        });

        tools.sort_by_key(|tool| {
            let name_match = tool.name.eq_ignore_ascii_case(query);
            (!name_match, tool.server.clone(), tool.name.clone())
        });

        Ok(tools)
    }

    /// 单个工具详情（命中缓存直接返回）。
    pub async fn describe_tool(
        &self,
        server_name: &str,
        tool_name: &str,
        cancel: &CancellationToken,
    ) -> Result<CachedTool> {
        if let Some(found) = self
            .inner
            .cache
            .lock()
            .await
            .servers
            .get(server_name)
            .and_then(|server| server.tools.iter().find(|tool| tool.name == tool_name))
            .cloned()
        {
            return Ok(found);
        }

        self.refresh_tools(server_name, cancel)
            .await?
            .into_iter()
            .find(|tool| tool.name == tool_name)
            .ok_or_else(|| McpError::ToolNotFound {
                server: server_name.to_string(),
                tool: tool_name.to_string(),
            })
    }

    /// 调用服务器工具。
    pub async fn call_tool(
        &self,
        server_name: &str,
        tool_name: &str,
        arguments: Value,
        cancel: &CancellationToken,
    ) -> Result<rmcp::model::CallToolResult> {
        let arguments = match arguments {
            Value::Object(arguments) => arguments,
            Value::Null => JsonObject::new(),
            _ => bail!("MCP tool arguments must be a JSON object"),
        };

        let runtime = self.runtime(server_name)?;
        let mut runtime = runtime.lock().await;
        let server = self.ensure_connected(server_name, &mut runtime).await?;

        let client = runtime
            .client
            .as_ref()
            .context("MCP client is not connected")?;

        let request = client
            .peer()
            .call_tool(CallToolRequestParams::new(tool_name.to_string()).with_arguments(arguments));

        let result = wait_request(request, self.timeout(server), cancel, "MCP tool call").await?;

        runtime.last_used = Some(Instant::now());
        drop(runtime);

        self.schedule_idle_disconnect(server_name, server);
        Ok(result)
    }

    /// 列出资源。
    pub async fn list_resources(
        &self,
        server_name: &str,
        cancel: &CancellationToken,
    ) -> Result<Vec<CachedResource>> {
        let runtime = self.runtime(server_name)?;
        let mut runtime = runtime.lock().await;
        let server = self.ensure_connected(server_name, &mut runtime).await?;

        let client = runtime
            .client
            .as_ref()
            .context("MCP client is not connected")?;

        let listed = wait_request(
            client.peer().list_all_resources(),
            self.timeout(server),
            cancel,
            "MCP resource list",
        )
        .await?;

        let resources = listed
            .into_iter()
            .map(|resource| CachedResource {
                server: server_name.to_string(),
                uri: resource.uri,
                name: resource.name,
                description: resource.description,
                mime_type: resource.mime_type,
            })
            .collect::<Vec<_>>();

        runtime.last_used = Some(Instant::now());
        drop(runtime);

        self.update_cache(server_name, server, |cache| {
            cache.resources = resources.clone()
        })
        .await;

        self.schedule_idle_disconnect(server_name, server);
        Ok(resources)
    }

    /// 读取资源。
    pub async fn read_resource(
        &self,
        server_name: &str,
        uri: &str,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        let runtime = self.runtime(server_name)?;
        let mut runtime = runtime.lock().await;
        let server = self.ensure_connected(server_name, &mut runtime).await?;

        let client = runtime
            .client
            .as_ref()
            .context("MCP client is not connected")?;

        let result = wait_request(
            client
                .peer()
                .read_resource(ReadResourceRequestParams::new(uri)),
            self.timeout(server),
            cancel,
            "MCP resource read",
        )
        .await?;

        runtime.last_used = Some(Instant::now());
        drop(runtime);

        self.schedule_idle_disconnect(server_name, server);
        Ok(serde_json::to_value(result)?)
    }

    /// 列出 prompts。
    pub async fn list_prompts(
        &self,
        server_name: &str,
        cancel: &CancellationToken,
    ) -> Result<Vec<CachedPrompt>> {
        let runtime = self.runtime(server_name)?;
        let mut runtime = runtime.lock().await;
        let server = self.ensure_connected(server_name, &mut runtime).await?;

        let client = runtime
            .client
            .as_ref()
            .context("MCP client is not connected")?;

        let listed = wait_request(
            client.peer().list_all_prompts(),
            self.timeout(server),
            cancel,
            "MCP prompt list",
        )
        .await?;

        let prompts = listed
            .into_iter()
            .map(|prompt| CachedPrompt {
                server: server_name.to_string(),
                name: prompt.name,
                title: prompt.title,
                description: prompt.description,
                arguments: serde_json::to_value(prompt.arguments).unwrap_or(Value::Null),
            })
            .collect::<Vec<_>>();

        runtime.last_used = Some(Instant::now());
        drop(runtime);

        self.update_cache(server_name, server, |cache| cache.prompts = prompts.clone())
            .await;
        self.schedule_idle_disconnect(server_name, server);
        Ok(prompts)
    }

    /// 获取 prompt。
    pub async fn get_prompt(
        &self,
        server_name: &str,
        prompt_name: &str,
        arguments: Value,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        let arguments = match arguments {
            Value::Object(arguments) => arguments,
            Value::Null => JsonObject::new(),
            _ => bail!("MCP prompt arguments must be a JSON object"),
        };

        let runtime = self.runtime(server_name)?;
        let mut runtime = runtime.lock().await;
        let server = self.ensure_connected(server_name, &mut runtime).await?;

        let client = runtime
            .client
            .as_ref()
            .context("MCP client is not connected")?;

        let result = wait_request(
            client
                .peer()
                .get_prompt(GetPromptRequestParams::new(prompt_name).with_arguments(arguments)),
            self.timeout(server),
            cancel,
            "MCP prompt request",
        )
        .await?;

        runtime.last_used = Some(Instant::now());
        drop(runtime);

        self.schedule_idle_disconnect(server_name, server);
        Ok(serde_json::to_value(result)?)
    }

    /// 主动断开某服务器，返回此前是否已连接。
    pub async fn disconnect(&self, server_name: &str) -> Result<bool> {
        let runtime = self.runtime(server_name)?;
        let mut runtime = runtime.lock().await;

        let Some(client) = runtime.client.take() else {
            return Ok(false);
        };

        _ = client.cancel().await;
        runtime.last_used = None;
        Ok(true)
    }

    /// 断开全部服务器。
    pub async fn disconnect_all(&self) {
        for name in self.inner.runtimes.keys() {
            _ = self.disconnect(name).await;
        }
    }

    /// 连上服务器列出全部工具，过滤掉未启用的，写回缓存并返回。
    ///
    /// `cancel` 触发时返回取消错误；列出后按服务器配置调度空闲断开。
    async fn refresh_tools(
        &self,
        server_name: &str,
        cancel: &CancellationToken,
    ) -> Result<Vec<CachedTool>> {
        let runtime = self.runtime(server_name)?;
        let mut runtime = runtime.lock().await;
        let server = self.ensure_connected(server_name, &mut runtime).await?;

        let client = runtime
            .client
            .as_ref()
            .context("MCP client is not connected")?;

        let listed = wait_request(
            client.peer().list_all_tools(),
            self.timeout(server),
            cancel,
            "MCP tool list",
        )
        .await?;

        let tools = listed
            .into_iter()
            .filter(|tool| tool_is_enabled(server, tool.name.as_ref()))
            .map(|tool| CachedTool {
                server: server_name.to_string(),
                name: tool.name.into_owned(),
                title: tool.title,
                description: tool.description.map(|value| value.into_owned()),
                input_schema: Value::Object((*tool.input_schema).clone()),
            })
            .collect::<Vec<_>>();

        runtime.last_used = Some(Instant::now());
        drop(runtime);

        self.update_cache(server_name, server, |cache| cache.tools = tools.clone())
            .await;
        self.schedule_idle_disconnect(server_name, server);
        Ok(tools)
    }

    /// 写入某服务器的元数据缓存；`server` 由已校验的调用路径传入。
    async fn update_cache(
        &self,
        server_name: &str,
        server: &ServerEntry,
        update: impl FnOnce(&mut ServerCache),
    ) {
        let mut cache = self.inner.cache.lock().await;
        let entry = cache
            .servers
            .entry(server_name.to_string())
            .or_insert_with(|| ServerCache {
                fingerprint: fingerprint(server),
                ..Default::default()
            });

        update(entry);
        cache.version = CACHE_VERSION;
        _ = save_cache(&self.inner.cache_path, &cache);
    }

    /// 配置里的服务器条目；`Err(ServerNotConfigured)` 代替对 `BTreeMap` 的索引 panic。
    fn server(&self, server_name: &str) -> Result<&ServerEntry> {
        self.inner
            .config
            .config
            .mcp_servers
            .get(server_name)
            .ok_or_else(|| McpError::ServerNotConfigured(server_name.to_string()))
    }

    /// 确保连接可用，并返回已校验存在的服务器配置条目。
    async fn ensure_connected(
        &self,
        server_name: &str,
        runtime: &mut Runtime,
    ) -> Result<&ServerEntry> {
        let server = self.server(server_name)?;

        if server.disabled {
            return Err(McpError::ServerDisabled(server_name.to_string()));
        }

        // 已连接但空闲超时：先断开再重连。
        if runtime.client.is_some()
            && runtime
                .last_used
                .is_some_and(|last| last.elapsed() >= idle_disconnect_after(server))
            && let Some(client) = runtime.client.take()
        {
            _ = client.cancel().await;
        }

        if runtime.client.is_some() {
            return Ok(server);
        }

        let result = connect(
            server_name,
            server,
            self.inner.credential_path.clone(),
            &self.inner.log_dir,
        )
        .await;

        match result {
            Ok(client) => {
                runtime.client = Some(client);
                runtime.last_used = Some(Instant::now());
                runtime.error = None;
                Ok(server)
            }
            Err(error) => {
                runtime.error = Some(error.to_string());
                Err(error)
            }
        }
    }

    /// 取服务器的共享运行态；未配置的服务器返回 `ServerNotConfigured`。
    fn runtime(&self, server_name: &str) -> Result<Arc<Mutex<Runtime>>> {
        self.inner
            .runtimes
            .get(server_name)
            .cloned()
            .ok_or_else(|| McpError::ServerNotConfigured(server_name.to_string()))
    }

    /// 解析要操作的服务器名列表：`one` 为 `Some` 时校验其存在且未禁用并只返回它，
    /// 为 `None` 时返回所有未禁用的服务器；指定的服务器不存在/被禁用时报错。
    fn server_names(&self, one: Option<&str>) -> Result<Vec<String>> {
        if let Some(name) = one {
            let server = self
                .inner
                .config
                .config
                .mcp_servers
                .get(name)
                .ok_or_else(|| McpError::ServerNotConfigured(name.to_string()))?;

            if server.disabled {
                return Err(McpError::ServerDisabled(name.to_string()));
            }

            return Ok(vec![name.to_string()]);
        }

        Ok(self
            .inner
            .config
            .config
            .mcp_servers
            .iter()
            .filter(|(_, server)| !server.disabled)
            .map(|(name, _)| name.clone())
            .collect())
    }

    /// 请求超时策略来自已校验的服务器条目。
    fn timeout(&self, server: &ServerEntry) -> Duration {
        server
            .request_timeout_ms
            .map(Duration::from_millis)
            .filter(|duration| !duration.is_zero())
            .unwrap_or(DEFAULT_TIMEOUT)
    }

    /// 空闲后自动断开；`server` 由已校验的调用路径传入。
    fn schedule_idle_disconnect(&self, server_name: &str, server: &ServerEntry) {
        if server.lifecycle.as_deref() == Some("persistent") {
            return;
        }

        let delay = idle_disconnect_after(server);
        let manager = self.clone();
        let name = server_name.to_string();

        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let Ok(runtime) = manager.runtime(&name) else {
                return;
            };

            let mut runtime = runtime.lock().await;
            if runtime
                .last_used
                .is_some_and(|last_used| last_used.elapsed() >= delay)
                && let Some(client) = runtime.client.take()
            {
                _ = client.cancel().await;
                runtime.last_used = None;
            }
        });
    }
}

/// 派生 stderr 日志目录：与缓存同级的 `logs/`。
fn log_dir_for(cache_path: &Path) -> PathBuf {
    match cache_path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join("logs"),
        _ => config::logs_dir(),
    }
}

/// stdio 服务器 stderr 的落点：追加写日志文件，打不开则丢弃。
fn stderr_log(path: &Path) -> Stdio {
    if let Some(parent) = path.parent() {
        _ = std::fs::create_dir_all(parent);
    }

    let opened = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|file| {
            if file.metadata()?.len() > STDERR_LOG_LIMIT_BYTES {
                std::fs::File::create(path)
            } else {
                Ok(file)
            }
        });

    match opened {
        Ok(file) => Stdio::from(file),
        Err(_) => Stdio::null(),
    }
}

/// 建立与服务器的连接：配了 `command` 就起 stdio 子进程（stderr 落到日志文件，
/// 超限时清空重写），否则走 Streamable HTTP（OAuth 模式要求已有登录凭据）。
/// 启动/初始化失败时返回 `Err`。
async fn connect(
    server_name: &str,
    server: &ServerEntry,
    credential_path: Option<PathBuf>,
    log_dir: &Path,
) -> Result<Client> {
    if let Some(command) = &server.command {
        let mut process = Command::new(expand_env(command));
        process.args(server.args.iter().map(|value| expand_env(value)));
        process.envs(
            server
                .env
                .iter()
                .map(|(key, value)| (key, expand_env(value))),
        );

        if let Some(cwd) = &server.cwd {
            process.current_dir(expand_home(&expand_env(cwd)));
        }

        // 子进程 stderr 必须脱离 prux 的终端：继承会让服务器日志直接写进 inline TUI 的滚动区，撕碎正在渲染的界面。
        let log_path = log_dir.join(format!("{server_name}.log"));
        let (transport, _stderr) = TokioChildProcess::builder(process)
            .stderr(stderr_log(&log_path))
            .spawn()
            .with_context(|| {
                format!(
                    "start MCP server `{server_name}` (stderr: {})",
                    log_path.display()
                )
            })?;
        return Ok(ClientInfo::default().serve(transport).await?);
    }

    ensure_tls_crypto_provider();

    let url = expand_env(server.url.as_deref().context("MCP URL is missing")?);

    // `auth: {"provider": ...}` 的 token 交给 `LiveTokenClient` 每次请求现取，静态配置里不填；
    // bearerToken / bearerTokenEnv 仍是建连时读一次的静态值。
    let static_bearer = match server.auth_provider() {
        Some(_) => None,
        None => bearer_token(server).await?,
    };
    let transport_config = http_transport_config(server, &url, static_bearer)?;

    if server.auth_mode() == Some(AuthMode::OAuth) {
        let oauth_config = server.oauth.clone().unwrap_or_default();
        let mut manager =
            oauth::new_manager(server_name, &url, &oauth_config, credential_path).await?;

        if !manager.initialize_from_store().await? {
            bail!("MCP server `{server_name}` needs OAuth login. Run `/mcp login {server_name}`")
        }

        let client = AuthClient::new(
            http_util::client_builder(None)
                .build()
                .context("build MCP auth HTTP client")?,
            manager,
        );
        let transport = StreamableHttpClientTransport::with_client(client, transport_config);
        return Ok(ClientInfo::default().serve(transport).await?);
    }

    let http_client = http_util::client_builder(None)
        .build()
        .context("build MCP HTTP client")?;
    if let Some(provider) = server.auth_provider() {
        let transport = StreamableHttpClientTransport::with_client(
            LiveTokenClient::new(http_client, Arc::from(provider)),
            transport_config,
        );
        return Ok(ClientInfo::default().serve(transport).await?);
    }

    let transport = StreamableHttpClientTransport::with_client(http_client, transport_config);
    Ok(ClientInfo::default().serve(transport).await?)
}

/// 探测服务器的 OAuth `WWW-Authenticate` 挑战（登录前调用）。
pub async fn probe_oauth_challenge(server: &ServerEntry) -> Result<Option<String>> {
    ensure_tls_crypto_provider();

    let url = expand_env(server.url.as_deref().context("MCP URL is missing")?);
    let transport = StreamableHttpClientTransport::with_client(
        http_util::client_builder(None)
            .build()
            .context("build MCP HTTP client")?,
        http_transport_config(server, &url, bearer_token(server).await?)?,
    );

    match ClientInfo::default().serve(transport).await {
        Ok(client) => {
            let _ = client.cancel().await;
            Ok(None)
        }
        Err(error) => match error.auth_challenge() {
            Some(challenge) => Ok(Some(challenge.to_string())),
            None => Err(error.into()),
        },
    }
}

/// 解析**建连时**要用的静态 bearer token：`bearerToken` / `bearerTokenEnv`，
/// 或 `probe_oauth_challenge` 沿用的 provider 登录 token；都不适用时返回 `Ok(None)`（匿名请求）。
///
/// `auth: {"provider": ...}` 的**实际请求**不走这里（token 由 `LiveTokenClient` 每次现取，
/// 见该类型的说明），只有 OAuth 挑战探测这类一次性用途会在这里读一次。
async fn bearer_token(server: &ServerEntry) -> Result<Option<String>> {
    if let Some(provider) = server.auth_provider() {
        let token = auth::resolve_provider_token(provider)
            .await
            .map_err(|error| {
                McpError::Message(format!("resolve provider token for `{provider}`: {error}"))
            })?
            .with_context(|| {
                format!(
                    "MCP server auth.provider `{provider}` has no credentials. Run `/login {provider}` first"
                )
            })?;
        return Ok(Some(token));
    }

    if server.auth_mode() != Some(AuthMode::Bearer) {
        return Ok(None);
    }

    let token = server
        .bearer_token
        .as_ref()
        .map(|value| expand_env(value))
        .or_else(|| {
            server
                .bearer_token_env
                .as_ref()
                .and_then(|name| std::env::var(name).ok())
        })
        .context("MCP bearer token is not configured")?;
    Ok(Some(token))
}

/// 每次请求前重新解析 `auth: {"provider": ...}` token 的 Streamable HTTP 客户端。
///
/// rmcp 的传输配置只接受静态 `Authorization`（建连时读一次），持久连接中途换 token 只能
/// `/mcp reload`；本包装把解析挪到每个请求之前，provider 的 OAuth token 快过期时也会先刷新。
#[derive(Clone)]
struct LiveTokenClient<C> {
    /// 内层真正发请求的客户端（`connect` 里传 `reqwest::Client`）。
    inner: C,
    /// 提供 token 的 provider 名。
    provider: Arc<str>,
}

impl<C> LiveTokenClient<C> {
    /// 包一层：请求交给 `inner`，token 每次现从 `provider` 取。
    fn new(inner: C, provider: Arc<str>) -> Self {
        Self { inner, provider }
    }
}

/// 把「取 token 失败」包成 rmcp 的传输错误：`Io` 变体可带任意消息，
/// 上浮后仍能看见 provider 名与处理指引。
fn live_token_error<E>(message: String) -> StreamableHttpError<E>
where
    E: std::error::Error + Send + Sync + 'static,
{
    StreamableHttpError::Io(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        message,
    ))
}

impl<C> LiveTokenClient<C>
where
    C: StreamableHttpClient + Send + Sync,
{
    /// 解析**本次请求**要用的 bearer token。
    ///
    /// provider 没有凭据时返回指引去 `/login` 的错误，凭据解析本身失败时带上失败原因；
    /// 两者都不会退化成匿名请求。
    async fn bearer(&self) -> std::result::Result<String, StreamableHttpError<C::Error>> {
        match auth::resolve_provider_token(&self.provider).await {
            Ok(Some(token)) => Ok(token),
            Ok(None) => Err(live_token_error(format!(
                "MCP server auth.provider `{}` has no credentials. Run `/login {}` first",
                self.provider, self.provider
            ))),
            Err(error) => Err(live_token_error(format!(
                "resolve provider token for `{}`: {error}",
                self.provider
            ))),
        }
    }
}

impl<C> StreamableHttpClient for LiveTokenClient<C>
where
    C: StreamableHttpClient + Send + Sync,
{
    type Error = C::Error;

    /// 每个请求前重取 token；`_auth_header`（传输配置里的静态值）被忽略。
    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> std::result::Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        let token = self.bearer().await?;
        self.inner
            .post_message(uri, message, session_id, Some(token), custom_headers)
            .await
    }

    /// 覆盖默认实现（默认体丢掉 `max_sse_event_size`），确保 SSE 上限不被绕开。
    async fn post_message_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> std::result::Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        let token = self.bearer().await?;
        self.inner
            .post_message_with_max_sse_event_size(
                uri,
                message,
                session_id,
                Some(token),
                custom_headers,
                max_sse_event_size,
            )
            .await
    }

    /// 断开会话（`DELETE`）也带实时 token：服务端可能靠它校验删除权限。
    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> std::result::Result<(), StreamableHttpError<Self::Error>> {
        let token = self.bearer().await?;
        self.inner
            .delete_session(uri, session_id, Some(token), custom_headers)
            .await
    }

    /// 服务器→客户端的 SSE 流：重连时同样用当下 token，而非建连时那个。
    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> std::result::Result<
        BoxStream<'static, std::result::Result<Sse, SseError>>,
        StreamableHttpError<Self::Error>,
    > {
        let token = self.bearer().await?;
        self.inner
            .get_stream(uri, session_id, last_event_id, Some(token), custom_headers)
            .await
    }

    /// 同 `post_message_with_max_sse_event_size`：覆盖默认实现以保留事件大小上限。
    async fn get_stream_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> std::result::Result<
        BoxStream<'static, std::result::Result<Sse, SseError>>,
        StreamableHttpError<Self::Error>,
    > {
        let token = self.bearer().await?;
        self.inner
            .get_stream_with_max_sse_event_size(
                uri,
                session_id,
                last_event_id,
                Some(token),
                custom_headers,
                max_sse_event_size,
            )
            .await
    }
}

/// 构造 Streamable HTTP 传输配置：展开并注入自定义 header，`token` 非空时附加 Authorization。
/// header 名/值非法时返回 `Err`。
fn http_transport_config(
    server: &ServerEntry,
    url: &str,
    token: Option<String>,
) -> Result<StreamableHttpClientTransportConfig> {
    let mut headers = HashMap::new();
    for (name, value) in &server.headers {
        headers.insert(
            HeaderName::try_from(name)?,
            HeaderValue::try_from(expand_env(value))?,
        );
    }

    let mut config = StreamableHttpClientTransportConfig::with_uri(url)
        .custom_headers(headers)
        .reinit_on_expired_session(true);

    if let Some(token) = token {
        config = config.auth_header(token);
    }

    Ok(config)
}

/// 在取消令牌与超时之间竞速地等待一个 RPC future。
///
/// 取消时返回 `McpError::Cancelled`，超时返回 `McpError::Timeout`，RPC 自身失败包装成 `McpError::Context`。
async fn wait_request<F, T, E>(
    future: F,
    timeout: Duration,
    cancel: &CancellationToken,
    label: &str,
) -> Result<T>
where
    F: std::future::Future<Output = std::result::Result<T, E>>,
    E: std::error::Error + Send + Sync + 'static,
{
    tokio::select! {
        _ = cancel.cancelled() => Err(McpError::Cancelled { label: label.to_string() }),
        result = tokio::time::timeout(timeout, future) => {
            result
                .map_err(|_| McpError::Timeout {
                    label: label.to_string(),
                    millis: timeout.as_millis(),
                })?
                .map_err(|error| McpError::Context {
                    context: label.to_string(),
                    source: Box::new(error),
                })
        }
    }
}

/// 工具是否通过 include/exclude 名单筛选：include 为空表示全含，命中 exclude 则排除。
fn tool_is_enabled(server: &ServerEntry, name: &str) -> bool {
    (server.include_tools.is_empty() || server.include_tools.iter().any(|item| item == name))
        && !server.exclude_tools.iter().any(|item| item == name)
}

/// 空闲多少分钟后自动断开（默认 10 分钟）。
///
/// 唯一计算点：`idle_timeout` 未做范围校验，用饱和乘法避免超大配置在 debug 构建下溢出 panic。
fn idle_disconnect_after(server: &ServerEntry) -> Duration {
    Duration::from_secs(server.idle_timeout.unwrap_or(10).saturating_mul(60))
}

/// 展开字符串里的 `${VAR}` 为环境变量值（未定义则替换为空串，未闭合的 `${` 原样保留）。
fn expand_env(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut rest = value;

    while let Some(start) = rest.find("${") {
        output.push_str(&rest[..start]);
        let tail = &rest[start + 2..];

        let Some(end) = tail.find('}') else {
            output.push_str(&rest[start..]);
            return output;
        };

        let name = &tail[..end];
        output.push_str(&std::env::var(name).unwrap_or_default());
        rest = &tail[end + 1..];
    }

    output.push_str(rest);
    output
}

/// 服务器配置序列化后的 SHA-256 十六进制指纹，用于判断缓存是否失效。
fn fingerprint(server: &ServerEntry) -> String {
    let encoded = serde_json::to_vec(server).unwrap_or_default();
    Sha256::digest(encoded)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// 从磁盘读取缓存：读取或解析失败返回 `Err`，版本号不匹配则返回空缓存。
fn load_cache(path: &Path) -> Result<CacheFile> {
    let text = std::fs::read_to_string(path)?;
    let cache: CacheFile = serde_json::from_str(&text)?;
    if cache.version != CACHE_VERSION {
        return Ok(CacheFile::default());
    }
    Ok(cache)
}

/// 同步读取缓存：每服务器的 (tools, resources, prompts) 计数（不连接）。
pub fn cache_counts(path: &Path) -> BTreeMap<String, (usize, usize, usize)> {
    load_cache(path)
        .map(|cache| {
            cache
                .servers
                .into_iter()
                .map(|(name, entry)| {
                    (
                        name,
                        (
                            entry.tools.len(),
                            entry.resources.len(),
                            entry.prompts.len(),
                        ),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 同步读取缓存里的工具元数据（不连接）。
pub fn cache_entries(path: &Path) -> BTreeMap<String, Vec<CachedTool>> {
    load_cache(path)
        .map(|cache| {
            cache
                .servers
                .into_iter()
                .map(|(name, entry)| (name, entry.tools))
                .collect()
        })
        .unwrap_or_default()
}

/// 原子写入缓存文件：自动创建父目录，先写临时文件再 rename 覆盖目标。
fn save_cache(path: &Path, cache: &CacheFile) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let temporary = path.with_extension(format!("json.tmp.{}", std::process::id()));
    let mut output = config::secure_create(&temporary)?;
    serde_json::to_writer_pretty(&mut output, cache)?;
    output.write_all(b"\n")?;
    output.sync_all()?;
    std::fs::rename(temporary, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::mcp::config::{ConfigPaths, McpConfig};
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    fn loaded(temp: &tempfile::TempDir, server: ServerEntry) -> LoadedConfig {
        let project = temp.path().join("project");
        let paths = ConfigPaths::for_roots(
            &temp.path().join("home"),
            &temp.path().join("home").join(".prux"),
            &project,
        );
        let mut config = McpConfig::default();
        config.mcp_servers.insert("demo".to_string(), server);
        LoadedConfig {
            config,
            sources: BTreeMap::new(),
            paths,
        }
    }

    #[tokio::test]
    async fn construction_is_lazy() {
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("started");
        let server = ServerEntry {
            command: Some("sh".to_string()),
            args: vec![
                "-c".to_string(),
                format!("touch {}; exit 1", marker.display()),
            ],
            ..Default::default()
        };
        let manager = McpManager::with_paths(
            loaded(&temp, server),
            temp.path().join("cache.json"),
            Some(temp.path().join("oauth.json")),
        );
        let status = manager.status().await;
        assert_eq!(status.len(), 1);
        assert!(!marker.exists(), "构造不应启动子进程");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stdio_fixture_lists_and_calls_a_tool() {
        let temp = tempfile::tempdir().unwrap();
        let script = r#"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"stdio-test","version":"1.0.0"}}}\n' "$id"
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo","description":"Echo text","inputSchema":{"type":"object"}}]}}\n' "$id"
      ;;
    *'"method":"tools/call"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"hello from stdio"}],"isError":false}}\n' "$id"
      ;;
  esac
done
"#;
        let manager = McpManager::with_paths(
            loaded(
                &temp,
                ServerEntry {
                    command: Some("sh".to_string()),
                    args: vec!["-c".to_string(), script.to_string()],
                    ..Default::default()
                },
            ),
            temp.path().join("cache.json"),
            Some(temp.path().join("oauth.json")),
        );
        let cancel = CancellationToken::new();
        let tools = manager.list_tools(Some("demo"), &cancel).await.unwrap();
        assert_eq!(tools[0].name, "echo");
        let result = manager
            .call_tool("demo", "echo", Value::Null, &cancel)
            .await
            .unwrap();
        assert_eq!(
            result.content[0].as_text().unwrap().text,
            "hello from stdio"
        );
        manager.disconnect_all().await;
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn stdio_server_stderr_is_redirected_to_log_file() {
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("fd2-target");
        let script = format!(
            r#"
readlink /proc/self/fd/2 > {marker}
printf 'boot noise from server\n' >&2
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      printf '{{"jsonrpc":"2.0","id":%s,"result":{{"protocolVersion":"2025-11-25","capabilities":{{"tools":{{}}}},"serverInfo":{{"name":"stdio-log","version":"1.0.0"}}}}}}\n' "$id"
      ;;
    *'"method":"tools/list"'*)
      printf '{{"jsonrpc":"2.0","id":%s,"result":{{"tools":[]}}}}\n' "$id"
      ;;
  esac
done
"#,
            marker = marker.display()
        );
        let manager = McpManager::with_paths(
            loaded(
                &temp,
                ServerEntry {
                    command: Some("sh".to_string()),
                    args: vec!["-c".to_string(), script],
                    ..Default::default()
                },
            ),
            temp.path().join("cache.json"),
            Some(temp.path().join("oauth.json")),
        );
        let cancel = CancellationToken::new();
        manager.list_tools(Some("demo"), &cancel).await.unwrap();

        let log = temp.path().join("logs").join("demo.log");
        let mut logged = String::new();
        for _ in 0..40 {
            logged = std::fs::read_to_string(&log).unwrap_or_default();
            if logged.contains("boot noise from server") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(
            logged.contains("boot noise from server"),
            "stderr 应落盘到日志文件，实际: {logged:?}"
        );
        let target = std::fs::read_to_string(&marker).unwrap();
        assert_eq!(
            target.trim(),
            log.to_string_lossy(),
            "子进程 stderr 不得继承 prux 的终端"
        );
        manager.disconnect_all().await;
    }

    async fn start_http_fixture() -> (String, Arc<AtomicUsize>, CancellationToken) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let request_count = requests.clone();
        let cancel = CancellationToken::new();
        let server_cancel = cancel.clone();
        tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    _ = server_cancel.cancelled() => break,
                    accepted = listener.accept() => accepted,
                };
                let Ok((mut socket, _)) = accepted else { break };
                request_count.fetch_add(1, Ordering::Relaxed);
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut buffer = [0_u8; 4096];
                    let header_end = loop {
                        let count = socket.read(&mut buffer).await.unwrap_or(0);
                        if count == 0 {
                            return;
                        }
                        request.extend_from_slice(&buffer[..count]);
                        if let Some(position) =
                            request.windows(4).position(|item| item == b"\r\n\r\n")
                        {
                            break position + 4;
                        }
                    };
                    let headers = String::from_utf8_lossy(&request[..header_end]);
                    let length = headers
                        .lines()
                        .find_map(|line| line.strip_prefix("Content-Length: "))
                        .or_else(|| {
                            headers
                                .lines()
                                .find_map(|line| line.strip_prefix("content-length: "))
                        })
                        .and_then(|value| value.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    while request.len() < header_end + length {
                        let count = socket.read(&mut buffer).await.unwrap_or(0);
                        if count == 0 {
                            break;
                        }
                        request.extend_from_slice(&buffer[..count]);
                    }
                    let value: Value =
                        serde_json::from_slice(&request[header_end..header_end + length])
                            .unwrap_or(Value::Null);
                    let Some(id) = value.get("id").cloned() else {
                        let _ = socket.write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                        return;
                    };
                    let method = value.get("method").and_then(Value::as_str).unwrap_or("");
                    let result = match method {
                        "initialize" => json!({
                            "protocolVersion": value.pointer("/params/protocolVersion").cloned().unwrap_or(json!("2025-11-25")),
                            "capabilities": {"tools": {}, "resources": {}, "prompts": {}},
                            "serverInfo": {"name": "prux-test", "version": "1.0.0"}
                        }),
                        "tools/list" => json!({"tools": [{
                            "name": "echo",
                            "description": "Echo a value",
                            "inputSchema": {"type": "object", "properties": {"value": {"type": "string"}}}
                        }]}),
                        "tools/call" => {
                            if value.pointer("/params/name").and_then(Value::as_str) == Some("slow")
                            {
                                tokio::time::sleep(Duration::from_millis(250)).await;
                            }
                            json!({"content": [{"type": "text", "text": "hello from MCP"}], "isError": false})
                        }
                        "resources/list" => {
                            json!({"resources": [{"uri": "test://note", "name": "note", "mimeType": "text/plain"}]})
                        }
                        "resources/read" => {
                            json!({"contents": [{"uri": "test://note", "mimeType": "text/plain", "text": "fixture resource"}]})
                        }
                        "prompts/list" => {
                            json!({"prompts": [{"name": "review", "description": "Review text"}]})
                        }
                        "prompts/get" => {
                            json!({"description": "Review text", "messages": [{"role": "user", "content": {"type": "text", "text": "review this"}}]})
                        }
                        _ => json!({}),
                    };
                    let body =
                        serde_json::to_vec(&json!({"jsonrpc": "2.0", "id": id, "result": result}))
                            .unwrap();
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.write_all(&body).await;
                });
            }
        });
        (format!("http://{address}/mcp"), requests, cancel)
    }

    #[tokio::test]
    async fn http_fixture_supports_tools_resources_and_prompts() {
        let temp = tempfile::tempdir().unwrap();
        let (url, requests, fixture_cancel) = start_http_fixture().await;
        let manager = McpManager::with_paths(
            loaded(
                &temp,
                ServerEntry {
                    url: Some(url),
                    ..Default::default()
                },
            ),
            temp.path().join("cache.json"),
            Some(temp.path().join("oauth.json")),
        );
        assert_eq!(requests.load(Ordering::Relaxed), 0);
        assert_eq!(manager.status().await.len(), 1);
        assert_eq!(requests.load(Ordering::Relaxed), 0);

        let cancel = CancellationToken::new();
        let tools = manager.list_tools(Some("demo"), &cancel).await.unwrap();
        assert_eq!(tools[0].name, "echo");
        let called = manager
            .call_tool("demo", "echo", json!({"value": "hello"}), &cancel)
            .await
            .unwrap();
        assert_eq!(called.content[0].as_text().unwrap().text, "hello from MCP");
        assert_eq!(
            manager.list_resources("demo", &cancel).await.unwrap()[0].name,
            "note"
        );
        let resource = manager
            .read_resource("demo", "test://note", &cancel)
            .await
            .unwrap();
        assert!(resource.to_string().contains("fixture resource"));
        assert_eq!(
            manager.list_prompts("demo", &cancel).await.unwrap()[0].name,
            "review"
        );
        let prompt = manager
            .get_prompt("demo", "review", Value::Null, &cancel)
            .await
            .unwrap();
        assert!(prompt.to_string().contains("review this"));

        let slow_cancel = CancellationToken::new();
        let trigger = slow_cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            trigger.cancel();
        });
        let error = manager
            .call_tool("demo", "slow", Value::Null, &slow_cancel)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("cancelled"));
        assert!(requests.load(Ordering::Relaxed) > 0);
        manager.disconnect_all().await;
        fixture_cancel.cancel();
    }

    #[test]
    fn environment_and_home_expansion_are_explicit() {
        unsafe { std::env::set_var("PRUX_MCP_TEST_VALUE", "value") };
        assert_eq!(expand_env("a-${PRUX_MCP_TEST_VALUE}-b"), "a-value-b");
        assert!(expand_home("~/work").ends_with("work"));
    }

    #[test]
    fn include_exclude_tool_filtering() {
        let mut server = ServerEntry::default();
        assert!(tool_is_enabled(&server, "anything"));
        server.include_tools = vec!["keep".to_string()];
        assert!(tool_is_enabled(&server, "keep"));
        assert!(!tool_is_enabled(&server, "other"));
        server.include_tools.clear();
        server.exclude_tools = vec!["drop".to_string()];
        assert!(!tool_is_enabled(&server, "drop"));
        assert!(tool_is_enabled(&server, "keep"));
    }
}
