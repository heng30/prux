//! 测试用 MCP 服务端：HTTP streamable MCP 端点 + 最小 OAuth 授权服务器端点。
//!
//! 给 `tests/mcp_oauth_e2e.rs` 提供真实对端，覆盖那些单测碰不到的运行时路径：
//!
//! - **2.11**（`auth: {"provider": ...}`）：`/mcp` 校验 `Authorization: Bearer <token>`，
//!   缺/错 token 时返回 401 + `WWW-Authenticate`（带 `resource_metadata` 指针）；
//! - **2.9**（CIMD）：授权服务器 metadata 宣告 `client_id_metadata_document_supported: true`，
//!   并记录 `/authorize` 收到的 `client_id`（CIMD 下应等于客户端的 metadata 文档 URL）；
//! - **2.10**（`authServerMetadataUrl`）：`/.well-known/oauth-authorization-server` **故意**返回
//!   缺 `token_endpoint` 的坏文档（发现流程必失败），`/metadata` 提供正确文档供配置覆盖。
//!
//! 本文件是**实现**：`examples/mcp_test_server/main.rs` 是它的可执行壳（手动起服务观察），
//! `tests/mcp_oauth_e2e.rs` 用 `#[path = "../examples/mcp_test_server/server.rs"]` 复用同一份实现。
//!
//! 放 examples 而非 `src/`：rmcp 的 server 侧 feature 与 `tower` 只在 dev-dependencies 里，
//! lib 本体不该被它们污染。

// 部分 API 只给 tests/examples 用（例如 authorize_requests）。
#![allow(dead_code)]
// rmcp 的结构体是 `#[non_exhaustive]`，只能 default + 字段赋值；
// 401 的 Err 分支直接回 Response（BoxBody），体积大但没必要为测试服务器包装。
#![allow(clippy::field_reassign_with_default, clippy::result_large_err)]

use std::{
    collections::BTreeMap,
    convert::Infallible,
    future::Future,
    sync::{Arc, Mutex},
};

use http_body_util::{BodyExt as _, Full, combinators::BoxBody};
use hyper::{
    Request, Response, StatusCode, body::Bytes, header, server::conn::http1, service::service_fn,
};
use hyper_util::rt::TokioIo;
use rmcp::{
    ErrorData as McpError, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
        JsonObject, ListToolsResult, PaginatedRequestParams, ProtocolVersion, ServerCapabilities,
        ServerInfo, Tool,
    },
    service::{MaybeSendFuture, RequestContext, RoleServer},
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    },
};
use serde_json::{Value, json};
use tokio::{net::TcpListener, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use tower::Service as _;

/// 测试服务器暴露的工具名（调用固定返回 [`PONG`]）。
pub const PING_TOOL: &str = "ping";
/// [`PING_TOOL`] 的固定返回值。
pub const PONG: &str = "pong";
/// 授权码流程里回给客户端的授权码（测试不做 `/token` 兑换）。
pub const AUTHORIZATION_CODE: &str = "test-code";

/// `/.well-known/oauth-authorization-server` 的响应方式。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DiscoveryMode {
    /// 404：客户端回落到「按 base URL 合成端点」的老行为。
    #[default]
    Missing,
    /// 返回**缺 `token_endpoint`** 的坏文档：发现流程必失败（2.10 的对照场景）。
    Broken,
    /// 返回完整文档（含 `client_id_metadata_document_supported`，2.9 的 CIMD 场景）。
    Valid,
}

/// 测试服务器开关。
#[derive(Debug, Clone, Default)]
pub struct TestServerOptions {
    /// `/mcp` 要求的 bearer token；`None` = 不校验 Authorization 头。
    pub required_token: Option<String>,
    /// 授权服务器元数据发现端点的行为。
    pub discovery: DiscoveryMode,
}

/// 已启动的测试服务器；`shutdown` 前一直监听 `127.0.0.1`。
pub struct TestMcpServer {
    /// 监听端口（`127.0.0.1`）。
    pub port: u16,
    /// 收到的 `/authorize` 查询参数，按到达顺序。
    authorize_requests: Arc<Mutex<Vec<BTreeMap<String, String>>>>,
    /// 运行中要求的 bearer token（`set_required_token` 可改）。
    required_token: Arc<Mutex<Option<String>>>,
    /// 关闭信号：`shutdown` 置位后 accept 循环退出。
    cancel: CancellationToken,
    /// accept 循环任务句柄。
    handle: JoinHandle<()>,
}

impl TestMcpServer {
    /// 在 `127.0.0.1` 的随机端口上启动服务器。
    pub async fn start(options: TestServerOptions) -> std::io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let port = listener.local_addr()?.port();
        let authorize_requests: Arc<Mutex<Vec<BTreeMap<String, String>>>> =
            Arc::new(Mutex::new(Vec::new()));
        // 运行时可改：验证客户端是否**每次请求**重读 token（而不仅仅是建连时读一次）。
        let required_token = Arc::new(Mutex::new(options.required_token.clone()));
        let cancel = CancellationToken::new();

        let mut config = StreamableHttpServerConfig::default();
        config.json_response = true;
        let mcp = StreamableHttpService::new(
            || Ok(TestHandler),
            Arc::new(LocalSessionManager::default()),
            config,
        );

        let state = ServerState {
            port,
            options,
            mcp,
            authorize_requests: authorize_requests.clone(),
            required_token: required_token.clone(),
        };
        let loop_cancel = cancel.clone();
        let handle = tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    _ = loop_cancel.cancelled() => break,
                    accepted = listener.accept() => accepted,
                };
                let Ok((stream, _)) = accepted else {
                    break;
                };
                let state = state.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |request| {
                        let state = state.clone();
                        async move { Ok::<_, Infallible>(state.route(request).await) }
                    });
                    _ = http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });

        Ok(Self {
            port,
            authorize_requests,
            required_token,
            cancel,
            handle,
        })
    }

    /// `/mcp` 端点 URL。
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/mcp", self.port)
    }

    /// `authServerMetadataUrl` 应指向的文档 URL（2.10 用的正确文档）。
    pub fn metadata_url(&self) -> String {
        format!("http://127.0.0.1:{}/metadata", self.port)
    }

    /// 已收到的 `/authorize` 请求的查询参数。
    pub fn authorize_requests(&self) -> Vec<BTreeMap<String, String>> {
        self.authorize_requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 改运行中要求的 bearer token（`None` = 不再校验）。
    ///
    /// 配合 `auth: {"provider": ...}` 的凭据轮换，用来验证客户端在**下一次请求**就换了 token。
    pub fn set_required_token(&self, token: Option<String>) {
        *self
            .required_token
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = token;
    }

    /// 停止服务器（幂等；不等待进行中的请求）。
    pub async fn shutdown(self) {
        self.cancel.cancel();
        _ = self.handle.await;
    }
}

/// 单个连接的共享状态。
#[derive(Clone)]
struct ServerState {
    /// 监听端口（拼回调/元数据 URL 用）。
    port: u16,
    /// 本次启动的开关。
    options: TestServerOptions,
    /// MCP 端点（rmcp 的 streamable HTTP tower service）。
    mcp: StreamableHttpService<TestHandler, LocalSessionManager>,
    /// `/authorize` 记录。
    authorize_requests: Arc<Mutex<Vec<BTreeMap<String, String>>>>,
    /// 要求的 bearer token（可运行时改）。
    required_token: Arc<Mutex<Option<String>>>,
}

impl ServerState {
    /// 按路径分发；未知路径返回 404。
    async fn route(
        &self,
        request: Request<hyper::body::Incoming>,
    ) -> Response<BoxBody<Bytes, Infallible>> {
        let path = request.uri().path().to_string();
        let query = request.uri().query().map(str::to_string);
        let base = format!("http://127.0.0.1:{}", self.port);

        match path.as_str() {
            "/mcp" => match self.check_bearer(&request) {
                Ok(()) => {
                    let mut service = self.mcp.clone();
                    service
                        .call(request)
                        .await
                        .unwrap_or_else(|_| json_response(StatusCode::INTERNAL_SERVER_ERROR, "{}"))
                }
                Err(response) => response,
            },
            // RFC 9728 的受保护资源元数据：这里不提供，让客户端回落到授权服务器元数据发现
            "/.well-known/oauth-protected-resource" => json_response(StatusCode::NOT_FOUND, "{}"),
            "/.well-known/oauth-authorization-server" => match self.options.discovery {
                DiscoveryMode::Missing => json_response(StatusCode::NOT_FOUND, "{}"),
                // 故意缺 `token_endpoint`：反序列化必失败，发现流程走不通
                DiscoveryMode::Broken => json_response(
                    StatusCode::OK,
                    json!({
                        "issuer": base,
                        "authorization_endpoint": format!("{base}/authorize"),
                        "registration_endpoint": format!("{base}/register"),
                    })
                    .to_string(),
                ),
                DiscoveryMode::Valid => {
                    json_response(StatusCode::OK, self.metadata_document().to_string())
                }
            },
            // 2.10 的配置文档：正确、完整
            "/metadata" => json_response(StatusCode::OK, self.metadata_document().to_string()),
            "/authorize" | "/oauth/authorize" => {
                if let Some(query) = query {
                    self.authorize_requests
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(parse_query(&query));
                }
                redirect_response(&self.authorize_redirect(&base))
            }
            "/register" => json_response(
                StatusCode::CREATED,
                json!({
                    "client_id": "registered-client",
                    "client_secret": "registered-secret",
                    "redirect_uris": [format!("{base}/callback")],
                })
                .to_string(),
            ),
            "/token" => json_response(
                StatusCode::OK,
                json!({
                    "access_token": "test-access-token",
                    "token_type": "Bearer",
                    "expires_in": 3600,
                    "refresh_token": "test-refresh-token",
                })
                .to_string(),
            ),
            _ => json_response(StatusCode::NOT_FOUND, "{}"),
        }
    }

    /// `/authorize` 收到的参数里带 `redirect_uri` 时按它 302 回授权码。
    fn authorize_redirect(&self, base: &str) -> String {
        let recorded = self
            .authorize_requests
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let redirect = recorded
            .last()
            .and_then(|params| params.get("redirect_uri").cloned())
            .unwrap_or_else(|| format!("{base}/callback"));
        let state = recorded
            .last()
            .and_then(|params| params.get("state").cloned())
            .unwrap_or_default();
        let separator = if redirect.contains('?') { '&' } else { '?' };
        format!("{redirect}{separator}code={AUTHORIZATION_CODE}&state={state}")
    }

    /// 授权服务器元数据文档：宣告支持 CIMD（2.9）与动态注册。
    ///
    /// 授权端点刻意用 `/oauth/authorize`（与发现/合成端点 `/authorize` 不同），
    /// 这样测试能凭 `authorization_url` 的路径判断用的是**配置的文档**还是发现结果（2.10）。
    fn metadata_document(&self) -> Value {
        let base = format!("http://127.0.0.1:{}", self.port);
        json!({
            "issuer": base,
            "authorization_endpoint": format!("{base}/oauth/authorize"),
            "token_endpoint": format!("{base}/token"),
            "registration_endpoint": format!("{base}/register"),
            "response_types_supported": ["code"],
            "code_challenge_methods_supported": ["S256"],
            // SEP-991：服务端宣告支持 URL 形式的 client_id，rmcp 才会走 CIMD 分支
            "client_id_metadata_document_supported": true,
        })
    }

    /// `/mcp` 的 bearer 校验；不通过时返回 401（带 `WWW-Authenticate`）。
    fn check_bearer(
        &self,
        request: &Request<hyper::body::Incoming>,
    ) -> Result<(), Response<BoxBody<Bytes, Infallible>>> {
        let expected = self
            .required_token
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let Some(expected) = expected else {
            return Ok(());
        };

        let actual = request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok());
        if actual == Some(format!("Bearer {expected}").as_str()) {
            return Ok(());
        }

        let challenge = format!(
            "Bearer realm=\"mcp\", resource_metadata=\"http://127.0.0.1:{}/.well-known/oauth-protected-resource\"",
            self.port
        );
        Err(Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .header(header::WWW_AUTHENTICATE, challenge)
            .header(header::CONTENT_TYPE, "application/json")
            .body(json_body(json!({ "error": "unauthorized" }).to_string()))
            .expect("build 401"))
    }
}

/// 最小 MCP 服务端实现：一个 [`PING_TOOL`]，调用固定返回 [`PONG`]。
#[derive(Clone)]
struct TestHandler;

impl ServerHandler for TestHandler {
    /// 服务器自述（名称/版本/能力）。
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.protocol_version = ProtocolVersion::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        let mut implementation = Implementation::default();
        implementation.name = "prux-mcp-test-server".to_string();
        implementation.version = "0.1.0".to_string();
        info.server_info = implementation;
        info.instructions = Some("prux MCP 端到端测试服务端".to_string());
        info
    }

    /// 只暴露一个无参 `ping` 工具。
    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, McpError>> + MaybeSendFuture + '_ {
        let schema: JsonObject = json!({ "type": "object", "properties": {} })
            .as_object()
            .cloned()
            .unwrap_or_default();
        let mut result = ListToolsResult::default();
        result.tools = vec![Tool::new(
            PING_TOOL,
            "Return a fixed reply",
            Arc::new(schema),
        )];
        std::future::ready(Ok(result))
    }

    /// `ping` 返回 [`PONG`]；其它工具名报 invalid_params。
    fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<CallToolResponse, McpError>> + MaybeSendFuture + '_ {
        let name = request.name.to_string();
        std::future::ready(if name == PING_TOOL {
            Ok(CallToolResponse::from(CallToolResult::success(vec![
                ContentBlock::text(PONG),
            ])))
        } else {
            Err(McpError::invalid_params(
                format!("unknown tool `{name}`"),
                None,
            ))
        })
    }
}

/// 构造 JSON 响应。
fn json_response(
    status: StatusCode,
    body: impl Into<String>,
) -> Response<BoxBody<Bytes, Infallible>> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(json_body(body.into()))
        .expect("build response")
}

/// 构造 302 重定向响应。
fn redirect_response(location: &str) -> Response<BoxBody<Bytes, Infallible>> {
    Response::builder()
        .status(StatusCode::FOUND)
        .header(header::LOCATION, location)
        .body(Full::new(Bytes::new()).boxed())
        .expect("build redirect")
}

/// 把字符串包成 boxed body。
fn json_body(body: impl Into<String>) -> BoxBody<Bytes, Infallible> {
    Full::new(Bytes::from(body.into())).boxed()
}

/// 解析 `k=v&k2=v2` 形式的查询串（含百分号解码：`redirect_uri` 一定是编码过的）。
fn parse_query(query: &str) -> BTreeMap<String, String> {
    url::form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect()
}
