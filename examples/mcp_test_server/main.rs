//! 手动起一个测试用 MCP 服务端：`cargo run --example mcp_test_server [-- --token SECRET]`。
//!
//! 实现见同目录的 `server.rs`（`tests/mcp_oauth_e2e.rs` 也复用它）。用途：
//!
//! - 观察 prux 对 HTTP MCP 服务器的真实请求（`Authorization`、`WWW-Authenticate`、initialize 握手）；
//! - 手动验证 2.9（CIMD）/2.10（`authServerMetadataUrl`）：`/authorize` 会打印收到的查询参数，
//!   `--broken-metadata` 让 `/.well-known/oauth-authorization-server` 返回缺 `token_endpoint` 的坏文档。
//!
//! 用法：
//! ```text
//! cargo run --example mcp_test_server -- --token sk-test
//! cargo run --example mcp_test_server -- --discovery broken
//! ```

#[path = "server.rs"]
mod server;

use std::{future, process};

#[tokio::main]
async fn main() {
    let mut options = server::TestServerOptions::default();
    let mut args = std::env::args().skip(1);
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--token" => options.required_token = args.next(),
            "--discovery" => {
                let value = args.next().unwrap_or_default();
                options.discovery = match value.as_str() {
                    "missing" => server::DiscoveryMode::Missing,
                    "broken" => server::DiscoveryMode::Broken,
                    "valid" => server::DiscoveryMode::Valid,
                    other => {
                        eprintln!("--discovery must be missing|broken|valid, got `{other}`");
                        process::exit(2);
                    }
                };
            }
            "--help" | "-h" => {
                println!(
                    "Usage: mcp_test_server [--token SECRET] [--discovery missing|broken|valid]\n\
                     \n\
                     --token SECRET     require `Authorization: Bearer SECRET` on /mcp\n\
                     --discovery MODE   /.well-known/oauth-authorization-server: missing (404, default),\n\
                     \x20                  broken (no token_endpoint), valid (full document with CIMD)"
                );
                return;
            }
            other => {
                eprintln!("unknown argument: {other} (try --help)");
                process::exit(2);
            }
        }
    }

    let running = match server::TestMcpServer::start(options).await {
        Ok(server) => server,
        Err(error) => {
            eprintln!("failed to start test MCP server: {error}");
            process::exit(1);
        }
    };

    println!("MCP endpoint:      {}", running.url());
    println!("metadata document: {}", running.metadata_url());
    println!(
        "authorize endpoint: http://127.0.0.1:{}/authorize",
        running.port
    );
    println!("Press Ctrl+C to stop.");

    // 没有启用 tokio 的 signal feature：挂起即可，Ctrl+C 直接结束进程
    future::pending::<()>().await;
    running.shutdown().await;
}
