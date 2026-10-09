//! MCP OAuth 2.1 编排与凭据持久化。
//!
//! 凭据文件为 `agent_dir()/extensions/mcp/oauth.json`，
//! 写入用进程 [`FileLock`] 保证读-改-写不丢更新。
//!
//! 支持两种授权：authorization code + PKCE（S256）与 client credentials；
//! token 自动刷新由 rmcp 的 `AuthorizationManager` 负责。
//!
//! **跨进程刷新串行化**：会轮换 refresh token 的服务器（如 Cloudflare）若被两个
//! 进程同时刷新，后写者会把先写者的新 token 覆盖掉，让授权作废。因此每个服务器
//! 各有一把刷新锁文件，刷新从「重读凭据」到「新凭据落盘」之间独占该锁：等锁的进程
//! 重读时发现 refresh token 已被换掉，就直接用落盘的新 token（不再发这次刷新）。
//! 见 [`RefreshCoordinator`] 与 [`RefreshSerializedOAuthHttpClient`]。

use super::config::{credentials_path, secure_create};
use crate::{
    APP_NAME,
    extensions::mcp::{
        config::{ClientRegistration, OAuthConfig, OAuthGrantType},
        ensure_tls_crypto_provider,
        error::{Context as _, Result, bail},
    },
    utils::{
        file_lock::{FileLock, LockWait},
        http as http_util,
    },
};
use async_trait::async_trait;
use rmcp::transport::auth::{
    AuthError, AuthorizationManager, AuthorizationMetadata, AuthorizationRequest,
    AuthorizationSession, ClientCredentialsConfig, CredentialStore, OAuthHttpClient,
    OAuthHttpClientError, OAuthHttpClientFuture, OAuthHttpRedirectPolicy, OAuthHttpRequest,
    OAuthState, StoredCredentials,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write as _,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

/// 默认回调地址（本地回环监听）。
pub const DEFAULT_REDIRECT_URI: &str = "http://127.0.0.1:3118/callback";

/// 刷新锁的等待总预算：比陈旧阈值长，保证崩溃遗留的锁能被后到者夺走后仍有机会取到。
const REFRESH_LOCK_WAIT: Duration = Duration::from_secs(25);
/// 刷新锁的重试间隔。
const REFRESH_LOCK_RETRY: Duration = Duration::from_millis(100);
/// 刷新锁文件的陈旧阈值（非 unix 后端据此回收崩溃遗留的锁）。
const REFRESH_LOCK_STALE_AGE: Duration = Duration::from_secs(20);
/// 单次刷新请求的超时：持锁期间不能让墙钟无界增长
const REFRESH_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// OAuth HTTP 请求的默认超时。把对授权服务器的每个请求限制在 15s，避免不响应的服务器拖住登录或刷新锁。
const OAUTH_HTTP_TIMEOUT: Duration = Duration::from_secs(15);
/// OAuth HTTP 响应体上限（与 rmcp 内置客户端一致）。
const MAX_OAUTH_RESPONSE_BYTES: usize = 1024 * 1024;

/// 凭据文件顶层结构（按服务器名索引）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct CredentialFile {
    /// 服务器名 → 该服务器的凭据。
    servers: BTreeMap<String, CredentialEnvelope>,
}

/// 单服务器的凭据信封：绑定 server URL，URL 变化则作废旧凭据。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CredentialEnvelope {
    /// 保存凭据时服务器的 URL。
    server_url: String,
    /// rmcp 持久化的 token / 客户端注册信息。
    credentials: StoredCredentials,
}

/// 读凭据文件；文件不存在视为空（首次登录）。
fn read_credential_file(path: &Path) -> Result<CredentialFile> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(CredentialFile::default());
        }
        Err(error) => {
            return Err(error).with_context(|| format!("read {}", path.display()));
        }
    };
    serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))
}

/// 原子写入凭据文件：进程级 [`FileLock`] + 临时文件 + rename，避免并发丢更新。
fn write_credential_file(path: &Path, credentials: &CredentialFile) -> Result<()> {
    let parent = path.parent().context("credential path has no parent")?;
    std::fs::create_dir_all(parent)?;

    let _lock = FileLock::acquire(path)?;
    let temporary = path.with_extension(format!("json.tmp.{}", std::process::id()));
    let result = (|| -> Result<()> {
        let mut output = secure_create(&temporary)?;
        serde_json::to_writer_pretty(&mut output, credentials)?;
        output.write_all(b"\n")?;
        output.sync_all()?;
        std::fs::rename(&temporary, path)?;
        Ok(())
    })();
    _ = std::fs::remove_file(temporary);

    result
}

/// 刷新锁文件路径：凭据文件的同目录 sidecar，按服务器 URL 的哈希分文件
/// （`...oauth.json.refresh-<hash>`；[`FileLock`] 再追加 `.lock`）。
fn refresh_lock_path(credential_path: &Path, server_url: &str) -> PathBuf {
    use sha2::{Digest as _, Sha256};
    let digest = Sha256::digest(server_url.as_bytes());
    let hash = digest
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let mut name = credential_path.as_os_str().to_os_string();
    name.push(format!(".refresh-{hash}"));
    PathBuf::from(name)
}

/// 跨进程刷新协调器：一次刷新从「重读凭据」到「新凭据落盘」之间独占该服务器的刷新锁。
/// 刷新 HTTP 客户端取锁，[`FileCredentialStore`] 的 `save()`/`clear()` 释放锁。
#[derive(Debug)]
struct RefreshCoordinator {
    /// 本次刷新已取得、待落盘后释放的锁（`None` 表示未持锁）。
    guard: Mutex<Option<FileLock>>,
    /// 刷新锁文件路径。
    lock_path: PathBuf,
    /// 凭据文件路径（锁内重读用）。
    credential_path: PathBuf,
    /// 服务器名（凭据文件内的索引键）。
    server_name: String,
    /// 服务器 URL（锁内重读时校验凭据仍绑定该 URL）。
    server_url: String,
}

impl RefreshCoordinator {
    /// 以凭据路径与服务信息构造协调器（锁文件路径由 [`refresh_lock_path`] 推导）。
    fn new(credential_path: PathBuf, server_name: &str, server_url: &str) -> Self {
        Self {
            guard: Mutex::new(None),
            lock_path: refresh_lock_path(&credential_path, server_url),
            credential_path,
            server_name: server_name.to_string(),
            server_url: server_url.to_string(),
        }
    }

    /// 取该服务器的刷新锁；本协调器已持锁时直接返回。
    /// 阻塞等待放到 `spawn_blocking` 里，不占用异步运行时的工作线程；
    /// `Err` 是展示给用户的失败原因（拿不到锁 / 阻塞任务失败）。
    async fn acquire(&self) -> std::result::Result<(), String> {
        if self
            .guard
            .lock()
            .map(|guard| guard.is_some())
            .unwrap_or(false)
        {
            return Ok(());
        }

        let path = self.lock_path.clone();
        let wait = LockWait {
            total: REFRESH_LOCK_WAIT,
            retry: REFRESH_LOCK_RETRY,
            stale_age: REFRESH_LOCK_STALE_AGE,
        };

        let guard = tokio::task::spawn_blocking(move || FileLock::acquire_waiting(&path, wait))
            .await
            .map_err(|error| format!("MCP OAuth refresh lock task failed: {error}"))?
            .map_err(|error| format!("MCP OAuth refresh lock failed: {error}"))?;

        if let Ok(mut slot) = self.guard.lock() {
            *slot = Some(guard);
        }
        Ok(())
    }

    /// 释放刷新锁（本协调器未持锁时为无操作）。
    fn release(&self) {
        if let Ok(mut slot) = self.guard.lock() {
            slot.take();
        }
    }

    /// 锁内重读已落盘的 token，返回 `(refresh_token, token 响应 JSON)`。
    /// 文件缺失 / 未绑定当前服务器 / 没有 token 响应或 refresh token 时返回 `None`。
    fn stored_tokens(&self) -> Option<(String, Vec<u8>)> {
        let file = read_credential_file(&self.credential_path).ok()?;
        let envelope = file.servers.get(&self.server_name)?;
        if envelope.server_url != self.server_url {
            return None;
        }
        let token = envelope.credentials.token_response.as_ref()?;
        let value = serde_json::to_value(token).ok()?;
        let refresh = value.get("refresh_token")?.as_str()?.to_string();
        Some((refresh, serde_json::to_vec(&value).ok()?))
    }
}

/// 文件型凭据存储：`agent_dir()/extensions/mcp/oauth.json`，按 `server_name` 索引并绑定 URL。
#[derive(Debug, Clone)]
pub struct FileCredentialStore {
    /// 凭据文件路径。
    path: PathBuf,
    /// 本存储对应的服务器名（文件内的索引键）。
    server_name: String,
    /// 本存储对应的服务器 URL（用于校验旧凭据是否仍适用）。
    server_url: String,
    /// 跨进程刷新协调器；`save()`/`clear()` 落盘后释放刷新锁。`None` 表示不参与串行化
    /// （例如 [`has_credentials`] / [`logout_with_path`] 这类不刷新 token 的调用）。
    coordinator: Option<Arc<RefreshCoordinator>>,
}

impl FileCredentialStore {
    /// 在默认凭据文件（`agent_dir()/extensions/mcp/oauth.json`）上打开该服务器的存储。
    pub fn discover(server_name: &str, server_url: &str) -> Result<Self> {
        Ok(Self::new(credentials_path(), server_name, server_url))
    }

    /// 在指定路径上构造存储（测试/自定义路径用）。
    /// `server_name`：文件内索引键；`server_url`：绑定校验用的服务器 URL。
    pub fn new(
        path: PathBuf,
        server_name: impl Into<String>,
        server_url: impl Into<String>,
    ) -> Self {
        Self {
            path,
            server_name: server_name.into(),
            server_url: server_url.into(),
            coordinator: None,
        }
    }

    /// 与 [`new`](Self::new) 相同，但挂上跨进程刷新协调器（刷新请求期间持锁）。
    fn with_coordinator(
        path: PathBuf,
        server_name: impl Into<String>,
        server_url: impl Into<String>,
        coordinator: Arc<RefreshCoordinator>,
    ) -> Self {
        Self {
            coordinator: Some(coordinator),
            ..Self::new(path, server_name, server_url)
        }
    }

    /// 读凭据文件；文件不存在视为空（首次登录）。
    fn read(&self) -> Result<CredentialFile> {
        read_credential_file(&self.path)
    }

    /// 原子写入凭据文件：进程级 FileLock + 临时文件 + rename，避免并发丢更新。
    fn write(&self, credentials: &CredentialFile) -> Result<()> {
        write_credential_file(&self.path, credentials)
    }
}

#[async_trait]
impl CredentialStore for FileCredentialStore {
    /// 读取本服务器凭据；URL 已变化则作废旧凭据并返回 None。
    async fn load(&self) -> std::result::Result<Option<StoredCredentials>, AuthError> {
        let mut file = self
            .read()
            .map_err(|error| AuthError::InternalError(error.to_string()))?;

        let Some(envelope) = file.servers.get(&self.server_name) else {
            return Ok(None);
        };

        if envelope.server_url == self.server_url {
            return Ok(Some(envelope.credentials.clone()));
        }

        // URL 变了：旧凭据作废。
        file.servers.remove(&self.server_name);
        self.write(&file)
            .map_err(|error| AuthError::InternalError(error.to_string()))?;

        Ok(None)
    }

    /// 写入本服务器凭据（连同当前 server URL）；落盘后释放刷新锁（若持有）。
    async fn save(&self, credentials: StoredCredentials) -> std::result::Result<(), AuthError> {
        let result = self.save_inner(credentials);
        if let Some(coordinator) = &self.coordinator {
            coordinator.release();
        }
        result
    }

    /// 删除本服务器凭据（不存在则为无操作），并释放刷新锁（若持有）。
    async fn clear(&self) -> std::result::Result<(), AuthError> {
        let result = self.clear_inner();
        if let Some(coordinator) = &self.coordinator {
            coordinator.release();
        }
        result
    }
}

impl FileCredentialStore {
    /// [`CredentialStore::save`] 的实际写入（不负责释放刷新锁）。
    fn save_inner(&self, credentials: StoredCredentials) -> std::result::Result<(), AuthError> {
        let mut file = self
            .read()
            .map_err(|error| AuthError::InternalError(error.to_string()))?;

        file.servers.insert(
            self.server_name.clone(),
            CredentialEnvelope {
                server_url: self.server_url.clone(),
                credentials,
            },
        );

        self.write(&file)
            .map_err(|error| AuthError::InternalError(error.to_string()))
    }

    /// [`CredentialStore::clear`] 的实际删除（不负责释放刷新锁）。
    fn clear_inner(&self) -> std::result::Result<(), AuthError> {
        let mut file = self
            .read()
            .map_err(|error| AuthError::InternalError(error.to_string()))?;
        if file.servers.remove(&self.server_name).is_some() {
            self.write(&file)
                .map_err(|error| AuthError::InternalError(error.to_string()))?;
        }
        Ok(())
    }
}

/// 默认 OAuth HTTP 执行器：与 rmcp 内置 reqwest 客户端等价（跟随/不跟随重定向两个客户端，
/// 30s 超时，响应体上限 1 MiB），供 [`RefreshSerializedOAuthHttpClient`] 在其上叠加串行化。
struct OAuthHttpExecutor {
    /// 跟随重定向的客户端（元数据发现等）。
    follow: reqwest::Client,
    /// 不跟随重定向的客户端（token 等敏感请求，防重定向泄露凭据）。
    stop: reqwest::Client,
}

impl OAuthHttpExecutor {
    /// 构造两个客户端；TLS 后端需调用方先 [`ensure_tls_crypto_provider`]。
    fn new() -> std::result::Result<Self, AuthError> {
        let build = |policy: reqwest::redirect::Policy| {
            http_util::client_builder(None)
                .timeout(OAUTH_HTTP_TIMEOUT)
                .redirect(policy)
                .build()
                .map_err(|error| AuthError::InternalError(error.to_string()))
        };
        Ok(Self {
            follow: build(reqwest::redirect::Policy::limited(10))?,
            stop: build(reqwest::redirect::Policy::none())?,
        })
    }

    /// 执行一次 OAuth HTTP 请求，返回状态/响应头/已缓冲的响应体。
    async fn send(
        &self,
        request: OAuthHttpRequest,
    ) -> std::result::Result<http::Response<Vec<u8>>, OAuthHttpClientError> {
        let boxed = |error: reqwest::Error| -> OAuthHttpClientError { Box::new(error) };
        let client = match request.redirect_policy {
            OAuthHttpRedirectPolicy::Follow => &self.follow,
            _ => &self.stop,
        };

        let request = reqwest::Request::try_from(request.request).map_err(boxed)?;
        let response = client.execute(request).await.map_err(boxed)?;
        let status = response.status();
        let version = response.version();
        let headers = response.headers().clone();

        if response
            .content_length()
            .is_some_and(|length| length > MAX_OAUTH_RESPONSE_BYTES as u64)
        {
            return Err(Box::new(std::io::Error::other(format!(
                "OAuth HTTP response body exceeds {MAX_OAUTH_RESPONSE_BYTES} bytes"
            ))));
        }

        let bytes = response.bytes().await.map_err(boxed)?;
        if bytes.len() > MAX_OAUTH_RESPONSE_BYTES {
            return Err(Box::new(std::io::Error::other(format!(
                "OAuth HTTP response body exceeds {MAX_OAUTH_RESPONSE_BYTES} bytes"
            ))));
        }

        let mut builder = http::Response::builder().status(status).version(version);
        for (name, value) in headers.iter() {
            builder = builder.header(name, value);
        }

        builder
            .body(bytes.to_vec())
            .map_err(|error| -> OAuthHttpClientError { Box::new(error) })
    }
}

impl OAuthHttpClient for OAuthHttpExecutor {
    /// 直接转发（无串行化），供非刷新请求使用。
    fn execute(&self, request: OAuthHttpRequest) -> OAuthHttpClientFuture<'_> {
        Box::pin(async move { self.send(request).await })
    }
}

/// 在 [`OAuthHttpExecutor`] 之上串行化 refresh grant 的客户端。
///
/// 刷新请求先取跨进程刷新锁，再在锁内重读凭据文件：refresh token 已被另一个进程换掉时
/// 直接用落盘的新 token 合成响应（不再发这次刷新，避免把轮换后的 refresh token 用废）；
/// 否则持锁发出请求，并在凭据 `save()` 落盘后才放锁（成功路径）；失败/超时立即放锁。
#[derive(Clone)]
struct RefreshSerializedOAuthHttpClient {
    /// 实际的 HTTP 执行器。
    inner: Arc<OAuthHttpExecutor>,
    /// 刷新锁协调器。
    coordinator: Arc<RefreshCoordinator>,
}

impl OAuthHttpClient for RefreshSerializedOAuthHttpClient {
    /// 见类型文档；非刷新请求直接转发。
    fn execute(&self, request: OAuthHttpRequest) -> OAuthHttpClientFuture<'_> {
        Box::pin(async move {
            let Some(presented) = refresh_grant_token(&request) else {
                return self.inner.send(request).await;
            };
            self.coordinator
                .acquire()
                .await
                .map_err(|error| -> OAuthHttpClientError {
                    Box::new(std::io::Error::other(error))
                })?;

            // 锁内重读：另一个进程已经刷新过 → 用它落盘的 token 合成响应
            if let Some((stored, body)) = self.coordinator.stored_tokens()
                && stored != presented
            {
                self.coordinator.release();
                return synthetic_token_response(body);
            }

            match tokio::time::timeout(REFRESH_REQUEST_TIMEOUT, self.inner.send(request)).await {
                // 成功：保持锁到 `FileCredentialStore::save()` 落盘后再释放
                Ok(Ok(response)) if response.status().is_success() => Ok(response),
                Ok(Ok(response)) => {
                    self.coordinator.release();
                    Ok(response)
                }
                Ok(Err(error)) => {
                    self.coordinator.release();
                    Err(error)
                }
                Err(_) => {
                    self.coordinator.release();
                    Err(Box::new(std::io::Error::other(format!(
                        "OAuth refresh request timed out after {}s",
                        REFRESH_REQUEST_TIMEOUT.as_secs()
                    ))))
                }
            }
        })
    }
}

/// 在 [`RefreshSerializedOAuthHttpClient`] 之上叠加取消：
/// 登录被取消（Esc / 会话关闭）时立即中断在途请求，而不是等它超时。
#[derive(Clone)]
struct CancelAwareOAuthHttpClient {
    /// 下层客户端（含刷新串行化）。
    inner: Arc<RefreshSerializedOAuthHttpClient>,
    /// 本次登录的取消信号。
    cancel: CancellationToken,
}

impl OAuthHttpClient for CancelAwareOAuthHttpClient {
    /// 请求与取消二选一；取消先到就立即返回取消错误。
    ///
    /// refresh grant 除外：它由下层按 [`REFRESH_REQUEST_TIMEOUT`] 限时，并在持跨进程刷新锁
    /// 期间发出；中途丢弃会把锁留在原地（只靠陈旧阈值回收），所以不在这里取消它。
    fn execute(&self, request: OAuthHttpRequest) -> OAuthHttpClientFuture<'_> {
        if refresh_grant_token(&request).is_some() {
            return self.inner.execute(request);
        }

        Box::pin(async move {
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => {
                    let error: OAuthHttpClientError =
                        Box::new(std::io::Error::other("MCP OAuth sign-in was cancelled"));
                    Err(error)
                }
                result = self.inner.execute(request) => result,
            }
        })
    }
}

/// 从 OAuth HTTP 请求体里取出 refresh grant 的 refresh token；非刷新请求返回 `None`。
fn refresh_grant_token(request: &OAuthHttpRequest) -> Option<String> {
    if request.request.method() != http::Method::POST {
        return None;
    }
    let mut is_refresh = false;
    let mut token = None;
    for (key, value) in url::form_urlencoded::parse(request.request.body()) {
        match key.as_ref() {
            "grant_type" => is_refresh = value == "refresh_token",
            "refresh_token" => token = Some(value.into_owned()),
            _ => {}
        }
    }
    if is_refresh { token } else { None }
}

/// 用已落盘的 token 响应合成一次 `200 OK`（token 端点响应形状）。
fn synthetic_token_response(
    body: Vec<u8>,
) -> std::result::Result<http::Response<Vec<u8>>, OAuthHttpClientError> {
    http::Response::builder()
        .status(http::StatusCode::OK)
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(body)
        .map_err(|error| -> OAuthHttpClientError { Box::new(error) })
}

/// 待完成的 authorization code 登录。
pub struct PendingLogin {
    /// rmcp 的授权状态机（持有 PKCE / CSRF 状态，仅内存态）。
    state: OAuthState,
    /// 待用户在浏览器中打开的授权 URL。
    pub authorization_url: String,
    /// 本次流程使用的回调地址。
    pub redirect_uri: String,
}

impl std::fmt::Debug for PendingLogin {
    /// 仅打印授权 URL 与回调地址，跳过内部 OAuthState（含 PKCE/CSRF 秘密）
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PendingLogin")
            .field("authorization_url", &self.authorization_url)
            .field("redirect_uri", &self.redirect_uri)
            .finish_non_exhaustive()
    }
}

/// 开始 authorization code + PKCE 登录，返回待打开的授权 URL。
pub async fn begin_login(
    server_name: &str,
    server_url: &str,
    oauth: &OAuthConfig,
    challenge: Option<&str>,
) -> Result<PendingLogin> {
    begin_login_with_path(server_name, server_url, oauth, challenge, None).await
}

/// [`begin_login`] 的带路径版本（测试/自定义凭据文件）。
/// `challenge`：可选挑战串，随授权请求一并提交（如资源指示）。
/// `credential_path`：None 用默认凭据文件。
pub async fn begin_login_with_path(
    server_name: &str,
    server_url: &str,
    oauth: &OAuthConfig,
    challenge: Option<&str>,
    credential_path: Option<PathBuf>,
) -> Result<PendingLogin> {
    begin_login_with_cancel(
        server_name,
        server_url,
        oauth,
        challenge,
        credential_path,
        None,
    )
    .await
}

/// [`begin_login_with_path`] 的带取消版本：`cancel` 触发时立即中断在途请求
///（元数据发现 / 动态注册 / 授权会话启动）。
pub async fn begin_login_with_cancel(
    server_name: &str,
    server_url: &str,
    oauth: &OAuthConfig,
    challenge: Option<&str>,
    credential_path: Option<PathBuf>,
    cancel: Option<CancellationToken>,
) -> Result<PendingLogin> {
    if oauth.grant_type != OAuthGrantType::AuthorizationCode {
        bail!("MCP server `{server_name}` does not use authorization_code login")
    }

    let redirect_uri = oauth
        .redirect_uri
        .clone()
        .unwrap_or_else(|| DEFAULT_REDIRECT_URI.to_string());

    let mut manager = new_manager_with_cancel(
        server_name,
        server_url,
        oauth,
        credential_path,
        cancel.clone(),
    )
    .await?;

    let mut request = AuthorizationRequest::new(&redirect_uri).with_client_name(
        oauth
            .client_name
            .as_deref()
            .unwrap_or(&format!("{APP_NAME} MCP Client")),
    );

    if let Some(scopes) = &oauth.scope {
        request = request.with_scopes(scopes.split_whitespace());
    }

    if let Some(client_id) = &oauth.client_id {
        request = request.with_preregistered_client(client_id);
    }

    if let Some(client_secret) = &oauth.client_secret {
        request = request.with_client_secret(client_secret);
    }

    if let Some(challenge) = challenge {
        request = request.with_challenge(challenge);
    }

    // CIMD：client_id 用本方的 metadata 文档 URL，省掉动态注册（服务端宣告支持时才生效）
    if oauth.client_registration == ClientRegistration::Cimd
        && let Some(document) = &oauth.client_metadata_url
    {
        request = request.with_client_metadata_url(document);
    }

    let state = match &oauth.auth_server_metadata_url {
        // 配了 metadata 文档就不再走发现流程。这里不能只 `set_metadata` 后调
        // `start_authorization`：后者会无条件重新发现并用发现结果覆盖已设的 metadata
        // （rmcp 3.1 的 `AuthorizationManager::start_authorization`），所以直接建授权会话。
        Some(metadata_url) => {
            manager
                .set_metadata(fetch_authorization_metadata(metadata_url, cancel.as_ref()).await?);
            let session = AuthorizationSession::new(manager, request)
                .await
                .map_err(|(_manager, error)| error)
                .context("start MCP OAuth authorization with configured metadata")?;
            OAuthState::Session(session)
        }
        None => {
            let mut state = OAuthState::Unauthorized(manager);
            state
                .start_authorization(request)
                .await
                .context("start MCP OAuth authorization")?;
            state
        }
    };

    let authorization_url = add_authorization_params(
        &state.get_authorization_url().await?,
        &oauth.authorization_params,
    )?;

    Ok(PendingLogin {
        state,
        authorization_url,
        redirect_uri,
    })
}

/// 用回调 URL 完成登录并落盘凭据。
pub async fn finish_login(mut pending: PendingLogin, callback_url: &str) -> Result<()> {
    pending
        .state
        .handle_callback_url(callback_url.trim())
        .await
        .context("complete MCP OAuth authorization")
}

/// client credentials 授权（无需浏览器）。
pub async fn login_client_credentials(
    server_name: &str,
    server_url: &str,
    oauth: &OAuthConfig,
) -> Result<()> {
    login_client_credentials_with_path(server_name, server_url, oauth, None).await
}

/// [`login_client_credentials`] 的带路径版本：把 clientId/clientSecret 换成 token 并落盘。
/// `credential_path`：None 用默认凭据文件。
pub async fn login_client_credentials_with_path(
    server_name: &str,
    server_url: &str,
    oauth: &OAuthConfig,
    credential_path: Option<PathBuf>,
) -> Result<()> {
    login_client_credentials_with_cancel(server_name, server_url, oauth, credential_path, None)
        .await
}

/// [`login_client_credentials_with_path`] 的带取消版本。
pub async fn login_client_credentials_with_cancel(
    server_name: &str,
    server_url: &str,
    oauth: &OAuthConfig,
    credential_path: Option<PathBuf>,
    cancel: Option<CancellationToken>,
) -> Result<()> {
    if oauth.grant_type != OAuthGrantType::ClientCredentials {
        bail!("MCP server `{server_name}` does not use client_credentials")
    }
    let client_id = oauth
        .client_id
        .clone()
        .context("client_credentials needs clientId")?;
    let client_secret = oauth
        .client_secret
        .clone()
        .context("client_credentials needs clientSecret")?;
    let scopes = oauth
        .scope
        .as_deref()
        .map(|value| value.split_whitespace().map(String::from).collect())
        .unwrap_or_default();
    let mut state = OAuthState::Unauthorized(
        new_manager_with_cancel(server_name, server_url, oauth, credential_path, cancel).await?,
    );

    state
        .authenticate_client_credentials(ClientCredentialsConfig::ClientSecret {
            client_id,
            client_secret,
            scopes,
            resource: Some(server_url.to_string()),
        })
        .await
        .context("authenticate MCP client credentials")
}

/// 拉取配置的授权服务器元数据文档（`oauth.authServerMetadataUrl`）。
///
/// 文档按 RFC 8414 / OIDC 形状解析（至少要有 `authorization_endpoint` 与 `token_endpoint`），
/// 拉不到或解析不了就报错——配置了该字段即表示「发现流程不可信」，静默回退发现只会掩盖问题。
/// `cancel` 触发时立即放弃在途请求。
async fn fetch_authorization_metadata(
    url: &str,
    cancel: Option<&CancellationToken>,
) -> Result<AuthorizationMetadata> {
    let request = async {
        http_util::client_builder(None)
            .build()
            .context("build MCP OAuth metadata client")?
            .get(url)
            .timeout(OAUTH_HTTP_TIMEOUT)
            .send()
            .await
            .with_context(|| format!("fetch MCP OAuth metadata from {url}"))
    };

    let response = match cancel {
        Some(cancel) => tokio::select! {
            biased;
            _ = cancel.cancelled() => bail!("MCP OAuth sign-in was cancelled"),
            result = request => result?,
        },
        None => request.await?,
    };

    let status = response.status();
    if !status.is_success() {
        bail!("MCP OAuth metadata {url} returned HTTP {status}")
    }

    response
        .json::<AuthorizationMetadata>()
        .await
        .with_context(|| format!("parse MCP OAuth metadata from {url}"))
}

/// 构造 rmcp 的 AuthorizationManager（已挂文件凭据存储）。
pub async fn new_manager(
    server_name: &str,
    server_url: &str,
    oauth: &OAuthConfig,
    credential_path: Option<PathBuf>,
) -> Result<AuthorizationManager> {
    new_manager_with_cancel(server_name, server_url, oauth, credential_path, None).await
}

/// [`new_manager`] 的带取消版本：传了 `cancel` 时，对授权服务器的每个请求都可被 Esc / 会话关闭立即中断。
pub async fn new_manager_with_cancel(
    server_name: &str,
    server_url: &str,
    oauth: &OAuthConfig,
    credential_path: Option<PathBuf>,
    cancel: Option<CancellationToken>,
) -> Result<AuthorizationManager> {
    ensure_tls_crypto_provider();

    let store = match credential_path {
        Some(path) => path,
        None => credentials_path(),
    };

    let coordinator = Arc::new(RefreshCoordinator::new(
        store.clone(),
        server_name,
        server_url,
    ));
    let serialized = RefreshSerializedOAuthHttpClient {
        inner: Arc::new(OAuthHttpExecutor::new().context("create MCP OAuth HTTP client")?),
        coordinator: coordinator.clone(),
    };
    let client: Arc<dyn OAuthHttpClient> = match cancel {
        Some(cancel) => Arc::new(CancelAwareOAuthHttpClient {
            inner: Arc::new(serialized),
            cancel,
        }),
        None => Arc::new(serialized),
    };

    let mut manager = AuthorizationManager::new_with_oauth_http_client(server_url, client)
        .await
        .context("create MCP OAuth manager")?;

    manager.set_allow_missing_issuer(oauth.skip_issuer_metadata_validation);

    manager.set_credential_store(FileCredentialStore::with_coordinator(
        store,
        server_name,
        server_url,
        coordinator,
    ));

    Ok(manager)
}

/// 是否已保存该服务器的凭据。
pub async fn has_credentials(server_name: &str, server_url: &str) -> Result<bool> {
    Ok(FileCredentialStore::discover(server_name, server_url)?
        .load()
        .await?
        .is_some())
}

/// 清除该服务器的凭据，返回此前是否存在。
pub async fn logout(server_name: &str, server_url: &str) -> Result<bool> {
    logout_with_path(server_name, server_url, None).await
}

/// [`logout`] 的带路径版本；`credential_path`：None 用默认凭据文件。
pub async fn logout_with_path(
    server_name: &str,
    server_url: &str,
    credential_path: Option<PathBuf>,
) -> Result<bool> {
    let store = match credential_path {
        Some(path) => FileCredentialStore::new(path, server_name, server_url),
        None => FileCredentialStore::discover(server_name, server_url)?,
    };

    let existed = store.load().await?.is_some();
    store.clear().await?;
    Ok(existed)
}

/// 把 config 里的自定义授权参数附加到授权 URL 上。
/// `url`：rmcp 生成的授权 URL；`params`：额外 query 参数。
/// 与流程自有参数（scope/state/code_challenge 等）同名时直接报错，防止覆盖 PKCE/CSRF 状态。
fn add_authorization_params(url: &str, params: &BTreeMap<String, String>) -> Result<String> {
    if params.is_empty() {
        return Ok(url.to_string());
    }

    let reserved: BTreeSet<&str> = [
        "scope",
        "state",
        "resource",
        "client_id",
        "redirect_uri",
        "response_type",
        "code_challenge",
        "code_challenge_method",
    ]
    .into_iter()
    .collect();

    let mut parsed = url::Url::parse(url)?;
    let mut query = parsed.query_pairs_mut();

    for (key, value) in params {
        if reserved.contains(key.as_str()) {
            bail!("OAuth authorizationParams cannot replace flow-owned `{key}`")
        }

        query.append_pair(key, value);
    }

    drop(query);

    Ok(parsed.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::AgentDirGuard;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio_util::sync::CancellationToken;

    /// 最小可用的凭据样本（测试用）。
    fn credentials() -> StoredCredentials {
        StoredCredentials::new("client".to_string(), None, vec![], None)
    }

    #[tokio::test]
    async fn credential_store_is_bound_to_server_url() {
        let _dir = AgentDirGuard::temp();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("oauth.json");
        let first = FileCredentialStore::new(path.clone(), "demo", "https://one.example/mcp");
        first.save(credentials()).await.unwrap();
        assert!(first.load().await.unwrap().is_some());

        let changed = FileCredentialStore::new(path, "demo", "https://two.example/mcp");
        assert!(changed.load().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn logout_clears_only_one_server() {
        let _dir = AgentDirGuard::temp();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("oauth.json");
        let first = FileCredentialStore::new(path.clone(), "one", "https://one.example/mcp");
        let second = FileCredentialStore::new(path, "two", "https://two.example/mcp");
        first.save(credentials()).await.unwrap();
        second.save(credentials()).await.unwrap();
        first.clear().await.unwrap();
        assert!(first.load().await.unwrap().is_none());
        assert!(second.load().await.unwrap().is_some());
    }

    #[test]
    fn provider_params_cannot_replace_pkce_state() {
        let mut params = BTreeMap::new();
        params.insert("state".to_string(), "attacker".to_string());
        assert!(add_authorization_params("https://example.com/auth?state=safe", &params).is_err());
    }

    /// 起一个本地假 OAuth 服务器（元数据 + /token），返回 (base url, 关闭用的 token)。
    async fn start_oauth_server() -> (String, CancellationToken) {
        crate::extensions::mcp::ensure_tls_crypto_provider();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let base = format!("http://{address}");
        let task_base = base.clone();
        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    _ = task_cancel.cancelled() => break,
                    accepted = listener.accept() => accepted,
                };
                let Ok((mut socket, _)) = accepted else {
                    break;
                };
                let base = task_base.clone();
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
                    let request_head = String::from_utf8_lossy(&request[..header_end]);
                    let target = request_head
                        .lines()
                        .next()
                        .and_then(|line| line.split_whitespace().nth(1))
                        .unwrap_or("/");
                    let (status, content_type, body) =
                        if target == "/.well-known/oauth-authorization-server" {
                            (
                                "200 OK",
                                "application/json",
                                serde_json::to_vec(&json!({
                                    "issuer": base,
                                    "authorization_endpoint": format!("{base}/authorize"),
                                    "token_endpoint": format!("{base}/token"),
                                    "response_types_supported": ["code"],
                                    "code_challenge_methods_supported": ["S256"]
                                }))
                                .unwrap(),
                            )
                        } else if target == "/token" {
                            (
                                "200 OK",
                                "application/json",
                                serde_json::to_vec(&json!({
                                    "access_token": "local-access-token",
                                    "token_type": "Bearer",
                                    "expires_in": 3600,
                                    "refresh_token": "local-refresh-token"
                                }))
                                .unwrap(),
                            )
                        } else {
                            ("404 Not Found", "text/plain", b"not found".to_vec())
                        };
                    let response = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.write_all(&body).await;
                });
            }
        });
        (base, cancel)
    }

    #[tokio::test]
    async fn authorization_code_flow_uses_pkce_rejects_wrong_state_and_saves_tokens() {
        let _dir = AgentDirGuard::temp();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("oauth.json");
        let (base, server_cancel) = start_oauth_server().await;
        let server_url = format!("{base}/mcp");
        let oauth = OAuthConfig {
            client_id: Some("prux-test-client".to_string()),
            scope: Some("tools.read".to_string()),
            ..Default::default()
        };

        let wrong = begin_login_with_path("demo", &server_url, &oauth, None, Some(path.clone()))
            .await
            .unwrap();
        let authorization = url::Url::parse(&wrong.authorization_url).unwrap();
        let params = authorization.query_pairs().collect::<BTreeMap<_, _>>();
        assert_eq!(params.get("code_challenge_method").unwrap(), "S256");
        assert!(!params.get("code_challenge").unwrap().is_empty());
        assert_eq!(params.get("scope").unwrap(), "tools.read");
        let wrong_callback = format!("{}?code=local-code&state=wrong-state", wrong.redirect_uri);
        assert!(finish_login(wrong, &wrong_callback).await.is_err());

        let pending = begin_login_with_path("demo", &server_url, &oauth, None, Some(path.clone()))
            .await
            .unwrap();
        let authorization = url::Url::parse(&pending.authorization_url).unwrap();
        let state = authorization
            .query_pairs()
            .find(|(key, _)| key == "state")
            .unwrap()
            .1
            .into_owned();
        let issuer = url::form_urlencoded::byte_serialize(base.as_bytes()).collect::<String>();
        let callback = format!(
            "{}?code=local-code&state={state}&iss={issuer}",
            pending.redirect_uri
        );
        finish_login(pending, &callback).await.unwrap();

        let store = FileCredentialStore::new(path, "demo", &server_url);
        let saved = store.load().await.unwrap().unwrap();
        assert!(saved.token_response.is_some());
        store.clear().await.unwrap();
        assert!(store.load().await.unwrap().is_none());
        server_cancel.cancel();
    }

    /// 会轮换 refresh token 的假 OAuth 服务器状态。
    #[derive(Debug, Default)]
    struct RotatingTokenServer {
        /// 已收到的 refresh grant 次数（在请求到达时自增，早于响应延迟）。
        grants: AtomicUsize,
        /// 每次 refresh grant 携带的 refresh token（按到达顺序）。
        presented: Mutex<Vec<String>>,
        /// 被拒绝的 refresh grant 次数（用了已被轮换掉的 refresh token）。
        rejected: AtomicUsize,
        /// 当前有效的 refresh token 序号（`refresh-<n>`）。
        issued: AtomicUsize,
    }

    /// 当前有效的 refresh token。
    fn current_refresh_token(server: &RotatingTokenServer) -> String {
        format!("refresh-{}", server.issued.load(Ordering::SeqCst))
    }

    /// 启动假 OAuth 服务器：元数据端点 + 会轮换 refresh token（并拒绝复用）的 token 端点。
    /// `/token` 上的 refresh grant 会延迟 300ms，给测试制造「两个进程同时刷新」的窗口。
    async fn start_rotating_oauth_server() -> (String, Arc<RotatingTokenServer>, CancellationToken)
    {
        crate::extensions::mcp::ensure_tls_crypto_provider();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(RotatingTokenServer::default());
        let cancel = CancellationToken::new();
        let task_state = state.clone();
        let task_base = base.clone();
        let task_cancel = cancel.clone();
        tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    _ = task_cancel.cancelled() => break,
                    accepted = listener.accept() => accepted,
                };
                let Ok((mut socket, _)) = accepted else {
                    break;
                };
                let state = task_state.clone();
                let base = task_base.clone();
                tokio::spawn(async move {
                    let _ = serve_rotating(&mut socket, &base, &state).await;
                });
            }
        });
        (base, state, cancel)
    }

    /// 处理一个假 OAuth 服务器的连接（读取完整请求，再按路径应答）。
    async fn serve_rotating(
        socket: &mut tokio::net::TcpStream,
        base: &str,
        state: &RotatingTokenServer,
    ) -> Option<()> {
        let (target, body) = read_http_request(socket).await?;
        if target == "/.well-known/oauth-authorization-server" {
            let payload = serde_json::to_vec(&json!({
                "issuer": base,
                "authorization_endpoint": format!("{base}/authorize"),
                "token_endpoint": format!("{base}/token"),
                "response_types_supported": ["code"],
                "code_challenge_methods_supported": ["S256"]
            }))
            .ok()?;
            return respond(socket, "200 OK", &payload).await;
        }

        if target == "/token" {
            let mut grant_type = String::new();
            let mut refresh_token = String::new();
            for (key, value) in url::form_urlencoded::parse(&body) {
                match key.as_ref() {
                    "grant_type" => grant_type = value.into_owned(),
                    "refresh_token" => refresh_token = value.into_owned(),
                    _ => {}
                }
            }
            if grant_type != "refresh_token" {
                return respond(socket, "400 Bad Request", b"unsupported grant").await;
            }

            state.grants.fetch_add(1, Ordering::SeqCst);
            state.presented.lock().unwrap().push(refresh_token.clone());
            if refresh_token != current_refresh_token(state) {
                state.rejected.fetch_add(1, Ordering::SeqCst);
                return respond(socket, "400 Bad Request", b"invalid_grant").await;
            }

            // 给「另一个进程同时在刷新」留出窗口
            tokio::time::sleep(Duration::from_millis(300)).await;
            let issued = state.issued.fetch_add(1, Ordering::SeqCst) + 1;
            let payload = serde_json::to_vec(&json!({
                "access_token": format!("access-{issued}"),
                "token_type": "Bearer",
                "expires_in": 3600,
                "refresh_token": format!("refresh-{issued}"),
            }))
            .ok()?;
            return respond(socket, "200 OK", &payload).await;
        }

        respond(socket, "404 Not Found", b"not found").await
    }

    /// 读一个 HTTP 请求（头部 + `Content-Length` 指定的 body），返回 `(target, body)`。
    async fn read_http_request(socket: &mut tokio::net::TcpStream) -> Option<(String, Vec<u8>)> {
        let mut buffer = Vec::new();
        let mut chunk = [0_u8; 4096];
        let header_end = loop {
            let count = socket.read(&mut chunk).await.ok()?;
            if count == 0 {
                return None;
            }
            buffer.extend_from_slice(&chunk[..count]);
            if let Some(position) = buffer.windows(4).position(|item| item == b"\r\n\r\n") {
                break position + 4;
            }
        };
        let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
        let target = head
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))?
            .to_string();
        let length = head
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())?
            })
            .unwrap_or(0);
        while buffer.len() < header_end + length {
            let count = socket.read(&mut chunk).await.ok()?;
            if count == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..count]);
        }
        Some((target, buffer[header_end..].to_vec()))
    }

    /// 写一个最简 HTTP 响应（连接关闭）。
    async fn respond(socket: &mut tokio::net::TcpStream, status: &str, body: &[u8]) -> Option<()> {
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        socket.write_all(response.as_bytes()).await.ok()?;
        socket.write_all(body).await.ok()?;
        Some(())
    }

    /// 造一份「access token 已过期、refresh token 为给定值」的凭据。
    fn expired_credentials(refresh_token: &str) -> StoredCredentials {
        let token: rmcp::transport::auth::OAuthTokenResponse = serde_json::from_value(json!({
            "access_token": "access-stale",
            "token_type": "Bearer",
            "expires_in": 3600,
            "refresh_token": refresh_token,
        }))
        .unwrap();
        let mut credentials = StoredCredentials::new(
            "prux-test-client".to_string(),
            Some(token),
            Vec::new(),
            None,
        );
        credentials.token_received_at = Some(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
                - 7200,
        );
        credentials
    }

    #[tokio::test]
    async fn refresh_is_serialized_across_managers_and_reuses_new_tokens() {
        let _dir = AgentDirGuard::temp();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("oauth.json");
        let (base, server, cancel) = start_rotating_oauth_server().await;
        let server_url = format!("{base}/mcp");
        let oauth = OAuthConfig {
            client_id: Some("prux-test-client".to_string()),
            ..Default::default()
        };

        // 两个「进程」（两个 manager）共用同一个凭据文件，初始都是已过期的旧 token
        let seed = FileCredentialStore::new(path.clone(), "demo", &server_url);
        seed.save(expired_credentials("refresh-0")).await.unwrap();

        let mut first = new_manager("demo", &server_url, &oauth, Some(path.clone()))
            .await
            .unwrap();
        let mut second = new_manager("demo", &server_url, &oauth, Some(path.clone()))
            .await
            .unwrap();
        assert!(first.initialize_from_store().await.unwrap());
        assert!(second.initialize_from_store().await.unwrap());

        // 第一个刷新先持锁并卡在假服务器的 300ms 延迟里；第二个随后发起，必须等锁
        let first_task = tokio::spawn(async move { first.get_access_token().await });
        while server.grants.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let second_token = second.get_access_token().await.unwrap();
        let first_token = first_task.await.unwrap().unwrap();

        assert_eq!(
            server.grants.load(Ordering::SeqCst),
            1,
            "只允许一次刷新请求"
        );
        assert_eq!(
            server.rejected.load(Ordering::SeqCst),
            0,
            "不得用已被轮换掉的 refresh token 再刷一次"
        );
        assert_eq!(server.presented.lock().unwrap().as_slice(), ["refresh-0"]);
        assert_eq!(first_token, "access-1");
        assert_eq!(second_token, "access-1", "后来者应使用已落盘的新 token");

        // 落盘的必须是最新凭据（否则下次连接又要重新登录）
        let stored = seed.load().await.unwrap().unwrap();
        let value = serde_json::to_value(stored.token_response.unwrap()).unwrap();
        assert_eq!(value["refresh_token"], "refresh-1");
        assert_eq!(value["access_token"], "access-1");

        cancel.cancel();
    }
}
