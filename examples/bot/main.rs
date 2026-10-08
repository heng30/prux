//! `bot` 客户端示例：一个 OpenAI 兼容客户端，用来打 `proxy` 扩展的本地网关。
//!
//! prux 侧准备（一次即可）：`/login <provider>` → `/extension` 开启 `proxy` →
//! `/proxy provider <provider>` → `/proxy listen 127.0.0.1:8765`。
//!
//! 运行：
//! ```text
//! RUST_LOG=debug cargo run --example bot
//! ```
//!
//! 环境变量：
//! - `BOT_DEMO_BASE_URL`：默认 `http://127.0.0.1:8765/v1`（proxy 网关）；
//! - `BOT_DEMO_MODEL`：默认 `deepseek-v4.1-flash`，须在目标 provider 的模型目录里精确命中；
//! - `BOT_DEMO_API_KEY`：默认空；网关配了 `api_key` / `PRUX_PROXY_TOKEN` 时填同一个值；
//! - `RUST_LOG=debug`：示例用 `log::debug!` 打印增量与完整回答（默认 level 是 error，什么都看不到）。

mod chat;
mod request;
mod response;

use chat::{Chat, ChatConfig};
use request::{APIConfig, HistoryChat};
use response::StreamTextItem;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Request Error {0}")]
    Request(#[from] reqwest::Error),
}

#[tokio::main]
async fn main() {
    env_logger::init();

    // 默认指向 proxy 扩展的本地网关；网关未配 api_key 时不需要 key
    let api_base_url = std::env::var("BOT_DEMO_BASE_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:8765/v1".to_string());
    let api_model =
        std::env::var("BOT_DEMO_MODEL").unwrap_or_else(|_| "deepseek-v4.1-flash".to_string());
    let api_key = std::env::var("BOT_DEMO_API_KEY").unwrap_or_default();

    let prompt = "Your are a chat bot.";
    let question = "给我一个Rust程序。要求中文输出。";

    log::info!("base_url={api_base_url} model={api_model}");

    let request_config = APIConfig {
        api_base_url,
        api_model,
        api_key,
        temperature: None,
    };

    let histories = vec![HistoryChat {
        utext: "hi".to_string(),
        btext: "Hello! 👋 How can I assist you today? 😊".to_string(),
    }];

    let (tx, mut rx) = tokio::sync::mpsc::channel::<StreamTextItem>(100);

    let chat_config = ChatConfig { tx };
    let chat = Chat::new(prompt, question, chat_config, request_config, histories);
    let mut content = String::new();

    let handle = tokio::spawn(async move {
        while let Some(item) = rx.recv().await {
            if let Some(ref text) = item.reasoning_text {
                content.push_str(&text);
            } else if let Some(ref text) = item.text {
                content.push_str(&text);
            }

            // 网关/上游失败（401 / 404 model_not_found / 上游 5xx / 流中途错误帧）走 etext；
            // error 级默认就打印，不需要 RUST_LOG
            if let Some(ref etext) = item.etext {
                log::error!("stream error: {etext}");
            }

            log::debug!("{item:?}");
        }

        log::debug!("{content}");
    });

    if let Err(e) = chat.start().await {
        log::warn!("Chat error: {e:?}");
    }

    _ = handle.await;
}
