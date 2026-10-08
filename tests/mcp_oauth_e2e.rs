//! MCP 端到端测试：真实 HTTP 服务端（`examples/mcp_test_server/server.rs`）+ prux 的真实客户端路径。
//!
//! 覆盖单测碰不到的运行时行为：
//!
//! - **2.11**：`auth: {"provider": ...}` 用 provider 的登录 token 作 bearer，连接并调用工具；
//!   换凭据后**下一次请求**就用新 token（不必 `/mcp reload`）；
//! - **2.10**：发现流程拿到坏 metadata 时失败，配 `oauth.authServerMetadataUrl` 后绕过发现；
//! - **2.9**：`oauth.clientRegistration: "cimd"` 时把 metadata 文档 URL 当 `client_id` 发出去。
//!
//! 服务端实现放在 `examples/` 而非 `src/`：rmcp 的 server 侧 feature 与 `tower` 只在
//! dev-dependencies 里，不该进 lib 本体的依赖图。

#[path = "../examples/mcp_test_server/server.rs"]
mod mcp_test_server;

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::OnceLock,
    time::Duration,
};

use mcp_test_server::{DiscoveryMode, TestMcpServer, TestServerOptions};
use prux::{
    core::auth,
    extensions::mcp::{
        config::{
            AuthSetting, ClientRegistration, ConfigPaths, LoadedConfig, McpConfig, OAuthConfig,
            ServerEntry,
        },
        manager::McpManager,
        oauth,
    },
};
use serde_json::json;
use tokio_util::sync::CancellationToken;

/// 本测试进程独占的 agent_dir（`PRUX_AGENT_DIR` 进程级 env）。
///
/// 不能用 `test_support::AgentDirGuard`：它是线程本地的，而 manager / OAuth 的
/// 网络任务跑在 tokio 的 worker 线程上，读不到 thread_local override。
fn agent_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("prux-mcp-e2e-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp agent dir");
        unsafe { std::env::set_var("PRUX_AGENT_DIR", &dir) };
        dir
    })
    .as_path()
}

/// 单台服务器的合并配置（不落盘，直接喂给 manager）。
fn loaded_with(server: ServerEntry) -> LoadedConfig {
    let dir = agent_dir();
    let mut config = McpConfig::default();
    config.mcp_servers.insert("demo".to_string(), server);
    LoadedConfig {
        config,
        sources: BTreeMap::new(),
        paths: ConfigPaths::for_roots(dir, dir, dir),
    }
}

/// 请求授权 URL 并返回 302 的 `Location`（不跟随重定向，回调地址没有真实监听）。
async fn follow_authorize(url: &str) -> String {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("http client");
    let response = client.get(url).send().await.expect("authorize request");
    assert_eq!(response.status(), 302, "授权端点应重定向");
    response
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string()
}

/// 取 URL 查询参数（测试只看 ASCII 明文参数，用 `url` crate 解百分号编码）。
fn query_param(url: &str, key: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    parsed
        .query_pairs()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.into_owned())
}

/// 2.11：`auth: {"provider": ...}` 把 provider 的登录 token 当 bearer 发出去。
#[tokio::test]
async fn provider_auth_connects_with_provider_token() {
    let dir = agent_dir();
    let server = TestMcpServer::start(TestServerOptions {
        required_token: Some("sk-mcp-token".to_string()),
        ..Default::default()
    })
    .await
    .expect("start test server");

    auth::write_auth_key("testprovider", "sk-mcp-token").expect("write auth key");

    let entry = ServerEntry {
        url: Some(server.url()),
        auth: Some(AuthSetting::Provider {
            provider: "testprovider".to_string(),
        }),
        ..Default::default()
    };
    entry.validate("demo").expect("配置应通过校验");

    let manager = McpManager::new(loaded_with(entry)).expect("manager");
    let cancel = CancellationToken::new();
    let tools = manager
        .list_tools(Some("demo"), &cancel)
        .await
        .expect("provider token 应能连上并列出工具");
    assert!(
        tools
            .iter()
            .any(|tool| tool.name == mcp_test_server::PING_TOOL),
        "{tools:?}"
    );

    let result = manager
        .call_tool("demo", mcp_test_server::PING_TOOL, json!({}), &cancel)
        .await
        .expect("调用工具");
    assert!(
        format!("{:?}", result.content).contains(mcp_test_server::PONG),
        "{:?}",
        result.content
    );
    manager.disconnect_all().await;

    // 未登录的 provider：连接前就报错，不该发匿名请求
    let anonymous = ServerEntry {
        url: Some(server.url()),
        auth: Some(AuthSetting::Provider {
            provider: "nobody".to_string(),
        }),
        ..Default::default()
    };
    let manager = McpManager::new(loaded_with(anonymous)).expect("manager");
    let error = manager
        .list_tools(Some("demo"), &cancel)
        .await
        .expect_err("未登录 provider 应失败");
    assert!(error.to_string().contains("nobody"), "{error}");
    server.shutdown().await;
    let _ = dir;
}

/// 2.11：`auth: {"provider": ...}` 的 token 在**每次请求**重读，不是建连时读一次。
///
/// 持久连接建立后再换凭据 + 服务端同步换校验值，下一发请求必须带新 token；
/// 建连时缓存 token 的旧实现会在这里被 401 卡住（得手动 `/mcp reload`）。
#[tokio::test]
async fn provider_auth_rereads_token_on_every_request() {
    let _dir = agent_dir();
    let server = TestMcpServer::start(TestServerOptions {
        required_token: Some("sk-first".to_string()),
        ..Default::default()
    })
    .await
    .expect("start test server");

    auth::write_auth_key("rotating", "sk-first").expect("write auth key");
    let entry = ServerEntry {
        url: Some(server.url()),
        auth: Some(AuthSetting::Provider {
            provider: "rotating".to_string(),
        }),
        ..Default::default()
    };
    entry.validate("demo").expect("配置应通过校验");

    let manager = McpManager::new(loaded_with(entry)).expect("manager");
    let cancel = CancellationToken::new();
    manager
        .list_tools(Some("demo"), &cancel)
        .await
        .expect("首个 token 应能连上");

    // 凭据与服务端校验值同时换成新 token：持久连接上的下一次请求要用新的。
    auth::write_auth_key("rotating", "sk-second").expect("rotate auth key");
    server.set_required_token(Some("sk-second".to_string()));

    let result = manager
        .call_tool("demo", mcp_test_server::PING_TOOL, json!({}), &cancel)
        .await
        .expect("换 token 后应立即生效，无需 /mcp reload");
    assert!(
        format!("{:?}", result.content).contains(mcp_test_server::PONG),
        "{:?}",
        result.content
    );

    // 凭据被删掉：请求应报错，而不是拿旧 token 继续跑或发匿名请求。
    auth::remove_auth("rotating").expect("remove auth key");
    let error = manager
        .call_tool("demo", mcp_test_server::PING_TOOL, json!({}), &cancel)
        .await
        .expect_err("凭据消失后应失败");
    assert!(error.to_string().contains("rotating"), "{error}");

    manager.disconnect_all().await;
    server.shutdown().await;
}

/// 2.10：发现流程拿到坏 metadata 时报错，配 `authServerMetadataUrl` 后跳过发现。
#[tokio::test]
async fn auth_server_metadata_url_overrides_broken_discovery() {
    let _dir = agent_dir();
    let server = TestMcpServer::start(TestServerOptions {
        discovery: DiscoveryMode::Broken,
        ..Default::default()
    })
    .await
    .expect("start test server");
    let credentials = tempfile::tempdir().expect("temp dir");

    // 对照：没有覆盖时，发现流程读到缺 token_endpoint 的文档 → rmcp 容错回落到端点合成，
    // 授权端点因此不是配置文档里的 `/oauth/authorize`。
    let plain = OAuthConfig::default();
    let discovered = oauth::begin_login_with_path(
        "demo",
        &server.url(),
        &plain,
        None,
        Some(credentials.path().join("oauth.json")),
    )
    .await
    .expect("rmcp 对坏文档容错，仍会合成端点");
    assert!(
        !discovered.authorization_url.contains("/oauth/authorize"),
        "未配置覆盖时不该用配置文档的端点: {}",
        discovered.authorization_url
    );

    // 配了 authServerMetadataUrl：直接拉该文档，授权端点来自文档
    let with_metadata = OAuthConfig {
        auth_server_metadata_url: Some(server.metadata_url()),
        ..Default::default()
    };
    let pending = oauth::begin_login_with_path(
        "demo",
        &server.url(),
        &with_metadata,
        None,
        Some(credentials.path().join("oauth.json")),
    )
    .await
    .expect("配置的 metadata 应让登录流程走下去");
    assert!(
        pending
            .authorization_url
            .starts_with(&format!("http://127.0.0.1:{}/oauth/authorize", server.port)),
        "{}",
        pending.authorization_url
    );

    // 授权端点确实可用：按授权 URL 请求会 302 回回调地址并带上授权码
    let redirect = follow_authorize(&pending.authorization_url).await;
    assert!(redirect.contains("code=test-code"), "{redirect}");
    server.shutdown().await;
}

/// 2.9：`clientRegistration: "cimd"` 时，`client_id` 就是本方的 metadata 文档 URL。
#[tokio::test]
async fn cimd_sends_metadata_document_as_client_id() {
    let _dir = agent_dir();
    let server = TestMcpServer::start(TestServerOptions {
        discovery: DiscoveryMode::Valid,
        ..Default::default()
    })
    .await
    .expect("start test server");
    let credentials = tempfile::tempdir().expect("temp dir");
    let document = "https://example.com/client-metadata.json";

    let oauth_config = OAuthConfig {
        client_registration: ClientRegistration::Cimd,
        client_metadata_url: Some(document.to_string()),
        ..Default::default()
    };
    let pending = oauth::begin_login_with_path(
        "demo",
        &server.url(),
        &oauth_config,
        None,
        Some(credentials.path().join("oauth.json")),
    )
    .await
    .expect("CIMD 登录流程应起得来");

    assert_eq!(
        query_param(&pending.authorization_url, "client_id").as_deref(),
        Some(document),
        "CIMD 下 client_id 应是 metadata 文档 URL: {}",
        pending.authorization_url
    );

    // 对照：默认（dynamic）注册走服务器的 /register，client_id 由服务器签发
    let dynamic = OAuthConfig::default();
    let dynamic_pending = oauth::begin_login_with_path(
        "demo",
        &server.url(),
        &dynamic,
        None,
        Some(credentials.path().join("oauth.json")),
    )
    .await
    .expect("动态注册登录流程应起得来");
    assert_eq!(
        query_param(&dynamic_pending.authorization_url, "client_id").as_deref(),
        Some("registered-client"),
        "{}",
        dynamic_pending.authorization_url
    );

    server.shutdown().await;
}

/// 服务端本身的自检：不校验 token 时匿名可连，校验时 401（免得测试失败时先怀疑 prux）。
#[tokio::test]
async fn test_server_enforces_bearer_when_configured() {
    let _dir = agent_dir();
    let server = TestMcpServer::start(TestServerOptions {
        required_token: Some("secret".to_string()),
        ..Default::default()
    })
    .await
    .expect("start test server");

    let entry = ServerEntry {
        url: Some(server.url()),
        auth: Some(AuthSetting::Disabled(false)),
        ..Default::default()
    };
    let manager = McpManager::new(loaded_with(entry)).expect("manager");
    let cancel = CancellationToken::new();
    let error = tokio::time::timeout(
        Duration::from_secs(20),
        manager.list_tools(Some("demo"), &cancel),
    )
    .await
    .expect("超时")
    .expect_err("缺 token 应被 401 拒绝");
    assert!(
        error.to_string().contains("401")
            || error.to_string().to_ascii_lowercase().contains("auth"),
        "{error}"
    );
    server.shutdown().await;
}
