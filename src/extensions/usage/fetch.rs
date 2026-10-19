//! `usage` 扩展的数据层：按 provider 拉取配额 / 限额用量，并把各家的响应解析成统一的窗口列表。
//!
//! 每个 provider 一个子模块（[`anthropic`] / [`openai`] / [`copilot`] / [`openrouter`] /
//! [`kimi`] / [`zai`] / [`deepseek`] / [`moonshot`]），本文件只保留三件事：
//! 统一的报告类型（[`ProviderReport`] / [`QuotaWindow`]）、端点清单与分派，
//! 以及各 provider 共用的凭据解析（[`token_for`]）；取 JSON 走 [`crate::utils::http::get_json`]。
//!
//! 解析全是**纯函数**，网络函数只负责取 JSON 再调用它，另有 `*_at(endpoint)` 变体供测试
//! 指向本地假服务，因此测试可以完全不联网。

mod anthropic;
mod common;
mod copilot;
mod deepseek;
mod kimi;
mod moonshot;
mod openai;
mod openrouter;
mod zai;

use crate::{
    core::{auth, login_registry},
    extensions::util::default_user_agent,
    utils::http,
};
use futures_util::future::join_all;
use reqwest::Client;
use std::time::Duration;

/// 单次配额请求的超时：这些端点不在请求热路径上，但要保证 `/usage` 不会长时间转圈。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// 有配额端点的 provider（顺序 = 面板里的展示顺序）。
pub(super) const SUPPORTED: &[&str] = &[
    "anthropic",
    "openai",
    "github-copilot",
    "openrouter",
    "kimi-coding",
    "zai",
    "zai-coding-cn",
    "deepseek",
    "moonshotai",
    "moonshotai-cn",
];

/// 一个配额窗口的用量。
#[derive(Debug, Clone, PartialEq)]
pub struct QuotaWindow {
    /// 窗口名（`5-hour` / `Weekly` / `Credits` / 模型名等），直接展示。
    pub name: String,
    /// 已用百分比，恒在 `0.0..=100.0`。
    pub percent: f32,
    /// 重置时刻（Unix 毫秒）；`None` 表示端点没给或给的不是可解析的时间。
    pub resets_at: Option<u64>,
}

/// 单个 provider 的配额报告。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProviderReport {
    /// provider 标识（`login_registry` 的 id）。
    pub provider: String,
    /// 面板上显示的 provider 名。
    pub label: String,
    /// 配额窗口（空 = 该 provider 只报了余额 / 键状态，没有可画的进度条）。
    pub windows: Vec<QuotaWindow>,
    /// 附加信息行（`(标题, 值)`），如余额、套餐名、重置提示。
    pub extra: Vec<(String, String)>,
    /// 拉取或解析失败的原因；`Some` 时窗口与附加信息一般为空。
    pub error: Option<String>,
}

impl ProviderReport {
    /// 新建一份空报告：只带 provider 身份。
    pub(super) fn new(provider: &str) -> Self {
        Self {
            provider: provider.to_string(),
            label: login_registry::provider_name(provider),
            ..Default::default()
        }
    }

    /// 新建一份只带错误信息的报告。
    pub(super) fn failed(provider: &str, error: impl Into<String>) -> Self {
        Self {
            error: Some(error.into()),
            ..Self::new(provider)
        }
    }
}

/// 已配置凭据、但本扩展没有配额端点的 provider（面板末行展示，避免「怎么没列出来」的疑问）。
pub(super) fn unqueried_providers() -> Vec<String> {
    auth::list_configured_providers()
        .into_iter()
        .filter(|id| login_registry::LOGIN_PROVIDERS.iter().any(|p| p.id == id))
        .filter(|id| !SUPPORTED.contains(&id.as_str()))
        .collect()
}

/// 并发拉取全部**已配置**且**有端点**的 provider，按 [`SUPPORTED`] 顺序返回。
///
/// 单个 provider 失败只影响它自己那一行（错误进 [`ProviderReport::error`]）。
pub(super) async fn fetch_all() -> Vec<ProviderReport> {
    let client = match build_client() {
        Ok(client) => client,
        Err(err) => {
            return vec![ProviderReport::failed("usage", err)];
        }
    };

    let configured = auth::list_configured_providers();
    let mut tasks = Vec::new();
    for (index, provider) in SUPPORTED.iter().enumerate() {
        if !configured.iter().any(|id| id == provider) {
            continue;
        }
        let client = client.clone();
        tasks.push(async move { (index, fetch_one(&client, provider).await) });
    }

    let mut out: Vec<(usize, ProviderReport)> = join_all(tasks)
        .await
        .into_iter()
        .filter_map(|(index, report)| report.map(|report| (index, report)))
        .collect();
    out.sort_by_key(|(index, _)| *index);
    out.into_iter().map(|(_, report)| report).collect()
}

/// 构造配额请求客户端：10s 超时、跟随重定向、走与其它扩展一致的代理与 UA 规则。
pub(super) fn build_client() -> Result<Client, String> {
    let user_agent = default_user_agent();
    http::build_client_with_options(&http::ClientOptions {
        timeout: REQUEST_TIMEOUT,
        user_agent: Some(&user_agent),
        ..http::ClientOptions::default()
    })
}

/// 按 provider 分派到各自的端点；没有可用凭据时返回 `None`（该 provider 不进报告）。
pub(super) async fn fetch_one(client: &Client, provider: &str) -> Option<ProviderReport> {
    match provider {
        "anthropic" => anthropic::fetch_anthropic(client).await,
        "openai" => openai::fetch_openai(client).await,
        "github-copilot" => copilot::fetch_copilot(client).await,
        "openrouter" => openrouter::fetch_openrouter(client).await,
        "kimi-coding" => kimi::fetch_kimi(client).await,
        "zai" | "zai-coding-cn" => zai::fetch_zai(client, provider).await,
        "deepseek" => deepseek::fetch_deepseek(client).await,
        "moonshotai" | "moonshotai-cn" => moonshot::fetch_moonshot(client, provider).await,
        _ => None,
    }
}

/// 取 provider 的可用令牌：OAuth 优先（过期先刷新），否则 auth.json / models.json / 环境变量的 key。
pub(super) async fn token_for(provider: &str) -> Result<Option<String>, String> {
    if let Some(token) = auth::ensure_oauth_valid(provider)
        .await
        .map_err(|err| err.to_string())?
    {
        return Ok(Some(token));
    }
    Ok(auth::resolve_api_key(provider, None))
}
