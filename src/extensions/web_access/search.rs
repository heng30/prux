//! Web 搜索 provider 路由与实现。
//!
//! 本阶段实现 8 个纯 REST provider：`tavily` `brave` `exa`
//! `jina` `searxng` `duckduckgo`（免密钥）`serper` `perplexity`。
//!
//! - `provider` 省略 / `auto` → 用配置的默认 provider，否则按偏好顺序取第一个可用；
//!   若所有需密钥的 provider 都不可用，则兜底到 `duckduckgo`；
//! - `provider: "all"` → 所有「可自动选中」的 provider 并发搜索后合并；
//!   显式专用 provider（本阶段为 `duckduckgo`）不参与 `all`；
//! - `provider: [a, b]` → 并发搜索指定 provider 并合并；
//! - 单个 provider 失败时按 `searchRouting.fallbackOn` / `searchRouting.providers`
//!   顺序回退（未配置回退时直接返回错误）。

use super::{
    super::util::default_user_agent,
    config::{self, Config, LoadedConfig},
};
use crate::{core::tools::ToolError, utils::http};
use futures_util::StreamExt;
use regex::Regex;
use serde_json::Value;
use std::{collections::HashSet, sync::OnceLock, time::Duration};
use strum::IntoEnumIterator;
use strum_macros::{EnumIter, EnumString, IntoStaticStr};

/// 单次搜索的选项
#[derive(Debug, Clone, Default)]
pub struct SearchOptions {
    /// 期望结果条数上限；`None` 时用 provider 默认值（当前为 5，内部限制在 1-20）
    pub num_results: Option<usize>,
    /// 时间范围过滤（如 `day` / `week` / `month` / `year`）；`None` 表示不限
    pub recency: Option<String>,
    /// 域名过滤：`-` 前缀表示排除，其余为包含
    pub domain_filter: Vec<String>,
}

/// 统一搜索结果条目
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SearchResult {
    /// 结果标题
    pub title: String,
    /// 结果链接（绝对 URL）
    pub url: String,
    /// 结果摘要片段
    pub snippet: String,
}

/// 统一搜索响应
#[derive(Debug, Clone)]
pub struct SearchResponse {
    /// provider 给出的聚合答案；多数 provider 为空
    pub answer: String,
    /// 合并后的搜索结果列表
    pub results: Vec<SearchResult>,
    /// 产出该响应的 provider 名称
    pub provider: String,
}

impl SearchResponse {
    /// 组装统一搜索响应；`provider` 记录产出者名称（并发聚合时为 `a+b`）。
    fn new(provider: &str, answer: String, results: Vec<SearchResult>) -> Self {
        SearchResponse {
            answer,
            results,
            provider: provider.to_string(),
        }
    }
}

/// 支持的 provider
///
/// 判别值即自动路由的偏好顺序（越小越优先），见 [`Provider::preference_rank`]；
/// 变体名与字符串的互转由 `strum` 派生（全小写、大小写不敏感），成员遍历由 [`Provider::iter`] 提供。
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumIter, EnumString, IntoStaticStr)]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
pub enum Provider {
    /// 自建 SearXNG（需 base URL）
    Searxng = 0,
    /// Exa（需 API key）
    Exa = 1,
    /// Brave Search（需 API key）
    Brave = 2,
    /// Tavily（需 API key）
    Tavily = 3,
    /// Serper（需 API key）
    Serper = 4,
    /// Jina Reader Search（需 API key）
    Jina = 5,
    /// Perplexity（需 API key）
    Perplexity = 6,
    /// DuckDuckGo HTML 端（免密钥，仅显式指定时使用）
    #[strum(serialize = "duckduckgo", serialize = "ddg")]
    DuckDuckGo = 7,
}

impl Provider {
    /// 解析 provider 名称（去首尾空白、大小写不敏感）
    pub fn parse(name: &str) -> Option<Provider> {
        name.trim().parse().ok()
    }

    /// 规范名称（`&'static str`）
    pub fn name(&self) -> &'static str {
        self.into()
    }

    /// 是否可自动选中（`auto` 路由与 `all` 聚合）。DuckDuckGo 为显式专用。
    pub fn auto_selectable(&self) -> bool {
        !matches!(self, Provider::DuckDuckGo)
    }

    /// 自动路由偏好顺序（越小越优先），由变体判别值决定
    pub fn preference_rank(&self) -> usize {
        *self as usize
    }

    /// 当前配置/环境下是否可用
    pub fn is_available(&self, cfg: &Config) -> bool {
        match self {
            Provider::DuckDuckGo => true,
            Provider::Tavily => {
                config::resolve_credential(cfg.tavily_api_key.as_deref(), "TAVILY_API_KEY")
                    .is_some()
            }
            Provider::Brave => {
                config::resolve_credential(cfg.brave_api_key.as_deref(), "BRAVE_API_KEY").is_some()
            }
            Provider::Exa => {
                config::resolve_credential(cfg.exa_api_key.as_deref(), "EXA_API_KEY").is_some()
            }
            Provider::Jina => {
                config::resolve_credential(cfg.jina_api_key.as_deref(), "JINA_API_KEY").is_some()
            }
            Provider::Searxng => cfg
                .searxng_base_url
                .as_deref()
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false),
            Provider::Serper => {
                config::resolve_credential(cfg.serper_api_key.as_deref(), "SERPER_API_KEY")
                    .is_some()
            }
            Provider::Perplexity => {
                config::resolve_credential(cfg.perplexity_api_key.as_deref(), "PERPLEXITY_API_KEY")
                    .is_some()
            }
        }
    }
}

/// 工具传入的 provider 选择
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderSelection {
    /// 未指定 / `auto`：用默认 provider，否则按偏好顺序取首个可用
    Auto,
    /// 指定单个 provider（名称，解析见 [`Provider::parse`]）
    Single(String),
    /// `all`：所有可自动选中的 provider 并发搜索后合并
    All,
    /// 并发搜索指定的一组 provider 后合并
    List(Vec<String>),
}

impl ProviderSelection {
    /// 从 JSON 参数解析 `provider`（字符串或字符串数组）。
    pub fn from_json(value: Option<&Value>) -> ProviderSelection {
        match value {
            None | Some(Value::Null) => ProviderSelection::Auto,
            Some(Value::String(s)) => {
                let s = s.trim();
                if s.is_empty() || s.eq_ignore_ascii_case("auto") {
                    ProviderSelection::Auto
                } else if s.eq_ignore_ascii_case("all") {
                    ProviderSelection::All
                } else {
                    ProviderSelection::Single(s.to_string())
                }
            }
            Some(Value::Array(items)) => {
                let list: Vec<String> = items
                    .iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                if list.is_empty() {
                    ProviderSelection::Auto
                } else {
                    ProviderSelection::List(list)
                }
            }
            _ => ProviderSelection::Auto,
        }
    }
}

/// 当前可用的 provider 列表，按偏好顺序返回。
pub fn available_providers(cfg: &Config) -> Vec<Provider> {
    let mut list: Vec<Provider> = Provider::iter().filter(|p| p.is_available(cfg)).collect();
    list.sort_by_key(|p| p.preference_rank());
    list
}

/// 解析路由候选：单个 provider 名称序列（用于 auto / 单 provider 回退）。
fn resolve_candidates(
    selection: &ProviderSelection,
    loaded: &LoadedConfig,
) -> Result<Vec<Provider>, ToolError> {
    let cfg = &loaded.config;
    match selection {
        ProviderSelection::All => {
            let list: Vec<Provider> = available_providers(cfg)
                .into_iter()
                .filter(|p| p.auto_selectable())
                .collect();
            if list.is_empty() {
                return Err(no_provider_error(cfg));
            }
            Ok(list)
        }
        ProviderSelection::List(names) => {
            let mut list = Vec::new();
            for name in names {
                let provider = Provider::parse(name).ok_or_else(|| {
                    ToolError(format!(
                        "Unknown search provider \"{name}\". Available: {}",
                        available_names(cfg)
                    ))
                })?;
                if !provider.is_available(cfg) {
                    return Err(ToolError(format!(
                        "Search provider \"{name}\" is not configured. Available: {}",
                        available_names(cfg)
                    )));
                }
                if !list.contains(&provider) {
                    list.push(provider);
                }
            }
            Ok(list)
        }
        ProviderSelection::Single(name) => {
            let provider = Provider::parse(name).ok_or_else(|| {
                ToolError(format!(
                    "Unknown search provider \"{name}\". Available: {}",
                    available_names(cfg)
                ))
            })?;
            if !provider.is_available(cfg) {
                return Err(ToolError(format!(
                    "Search provider \"{name}\" is not configured. Available: {}",
                    available_names(cfg)
                )));
            }
            let mut list = vec![provider];
            for fallback in fallback_providers(cfg, provider) {
                if !list.contains(&fallback) {
                    list.push(fallback);
                }
            }
            Ok(list)
        }
        ProviderSelection::Auto => {
            let mut list: Vec<Provider> = Vec::new();
            if let Some(configured) = cfg
                .provider
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                && !configured.eq_ignore_ascii_case("auto")
            {
                if let Some(provider) = Provider::parse(configured) {
                    if provider.is_available(cfg) {
                        list.push(provider);
                    }
                } else if !configured.eq_ignore_ascii_case("all") {
                    return Err(ToolError(format!(
                        "Unknown configured provider \"{configured}\". Available: {}",
                        available_names(cfg)
                    )));
                }
            }
            // searchRouting.providers 明确顺序优先
            for name in &cfg.search_routing.providers {
                if let Some(provider) = Provider::parse(name)
                    && provider.is_available(cfg)
                    && !list.contains(&provider)
                {
                    list.push(provider);
                }
            }
            // 全部可自动选中的可用 provider 按偏好顺序
            for provider in available_providers(cfg) {
                if provider.auto_selectable() && !list.contains(&provider) {
                    list.push(provider);
                }
            }
            // 兜底：无任何可用 provider 时，使用免密钥的 DuckDuckGo
            if list.is_empty() && Provider::DuckDuckGo.is_available(cfg) {
                list.push(Provider::DuckDuckGo);
            }
            if list.is_empty() {
                return Err(no_provider_error(cfg));
            }
            Ok(list)
        }
    }
}

/// 串行回退候选：取 `search_routing.providers` 中除主 provider 外当前可用的 provider，按配置顺序排列。
fn fallback_providers(cfg: &Config, primary: Provider) -> Vec<Provider> {
    let mut list = Vec::new();
    for name in &cfg.search_routing.providers {
        if let Some(provider) = Provider::parse(name)
            && provider != primary
            && provider.is_available(cfg)
        {
            list.push(provider);
        }
    }
    list
}

/// 逗号拼接当前可用的 provider 名供错误信息展示；无可用时返回 `none configured`。
fn available_names(cfg: &Config) -> String {
    let names: Vec<&str> = available_providers(cfg).iter().map(|p| p.name()).collect();
    if names.is_empty() {
        "none configured".to_string()
    } else {
        names.join(", ")
    }
}

/// 无任何可用 provider 时的统一报错，提示可配置的 API key 环境变量与配置文件路径。
fn no_provider_error(_cfg: &Config) -> ToolError {
    ToolError(format!(
        "No search provider is configured. Set an API key in {} or the matching env var \
         (TAVILY_API_KEY / BRAVE_API_KEY / EXA_API_KEY / JINA_API_KEY / SERPER_API_KEY / \
         PERPLEXITY_API_KEY), or set searxngBaseUrl.",
        config::config_path().display()
    ))
}

/// 执行搜索。`selection` 为工具参数；返回合并后的响应。
pub async fn search(
    query: &str,
    selection: &ProviderSelection,
    options: &SearchOptions,
    loaded: &LoadedConfig,
    proxy: Option<&str>,
) -> Result<SearchResponse, ToolError> {
    let query = query.trim();
    if query.is_empty() {
        return Err(ToolError("Search query is empty".to_string()));
    }
    let candidates = resolve_candidates(selection, loaded)?;
    let cfg = &loaded.config;

    // 单候选：串行回退；多候选（all / 数组）：并发聚合
    if candidates.len() == 1 {
        return run_with_fallback(query, &candidates, options, cfg, proxy).await;
    }
    if matches!(
        selection,
        ProviderSelection::All | ProviderSelection::List(_)
    ) {
        return run_parallel(query, &candidates, options, cfg, proxy).await;
    }
    run_with_fallback(query, &candidates, options, cfg, proxy).await
}

/// 按候选顺序逐个搜索，返回首个成功结果；全部失败时返回最后一个错误。
async fn run_with_fallback(
    query: &str,
    candidates: &[Provider],
    options: &SearchOptions,
    cfg: &Config,
    proxy: Option<&str>,
) -> Result<SearchResponse, ToolError> {
    let mut last_error: Option<ToolError> = None;
    for provider in candidates {
        match run_single(*provider, query, options, cfg, proxy).await {
            Ok(response) => return Ok(response),
            Err(err) => {
                last_error = Some(err);
            }
        }
    }
    Err(last_error.unwrap_or_else(|| ToolError("No search provider available".to_string())))
}

/// 并发（最多 3 路）搜索所有候选并合并：答案按顺序拼接，结果按 URL 去重，`provider` 记为 `a+b`；
/// 全部失败时报 `All providers failed`。
async fn run_parallel(
    query: &str,
    candidates: &[Provider],
    options: &SearchOptions,
    cfg: &Config,
    proxy: Option<&str>,
) -> Result<SearchResponse, ToolError> {
    let concurrency = 3usize;
    let results: Vec<Result<SearchResponse, ToolError>> = futures_util::stream::iter(
        candidates
            .iter()
            .copied()
            .map(|provider| run_single(provider, query, options, cfg, proxy)),
    )
    .buffered(concurrency)
    .collect()
    .await;

    let mut answers: Vec<String> = Vec::new();
    let mut merged: Vec<SearchResult> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut providers: Vec<&str> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    for (provider, result) in candidates.iter().zip(results) {
        match result {
            Ok(response) => {
                providers.push(provider.name());
                if !response.answer.trim().is_empty() {
                    answers.push(response.answer);
                }
                for item in response.results {
                    if seen.insert(item.url.clone()) {
                        merged.push(item);
                    }
                }
            }
            Err(err) => errors.push(format!("{}: {}", provider.name(), err.0)),
        }
    }
    if providers.is_empty() {
        return Err(ToolError(format!(
            "All providers failed: {}",
            errors.join("; ")
        )));
    }
    Ok(SearchResponse {
        answer: answers.join("\n\n"),
        results: merged,
        provider: providers.join("+"),
    })
}

/// 建好 HTTP client 后按 provider 分派到各自的请求实现。
async fn run_single(
    provider: Provider,
    query: &str,
    options: &SearchOptions,
    cfg: &Config,
    proxy: Option<&str>,
) -> Result<SearchResponse, ToolError> {
    let client = build_client(cfg, proxy)?;
    match provider {
        Provider::Tavily => tavily(&client, cfg, query, options).await,
        Provider::Brave => brave(&client, cfg, query, options).await,
        Provider::Exa => exa(&client, cfg, query, options).await,
        Provider::Jina => jina(&client, cfg, query, options).await,
        Provider::Searxng => searxng(&client, cfg, query, options).await,
        Provider::DuckDuckGo => duckduckgo(&client, query, options).await,
        Provider::Serper => serper(&client, cfg, query, options).await,
        Provider::Perplexity => perplexity(&client, cfg, query, options).await,
    }
}

/// 构造带超时/UA/代理的 reqwest client；`proxy` 参数优先于配置里的 proxy，空串视为未设置。
fn build_client(cfg: &Config, proxy: Option<&str>) -> Result<reqwest::Client, ToolError> {
    let proxy = proxy
        .map(str::to_string)
        .or_else(|| cfg.proxy.clone())
        .filter(|p| !p.trim().is_empty());
    let user_agent = cfg.user_agent.clone().unwrap_or_else(default_user_agent);
    http::build_client_with_options(&http::ClientOptions {
        timeout: config::http_timeout(cfg),
        connect_timeout: Some(Duration::from_secs(15)),
        user_agent: Some(&user_agent),
        proxy: proxy.as_deref(),
        pool_max_idle_per_host: 4,
        ..Default::default()
    })
    .map_err(ToolError)
}

/// 结果条数：未指定时默认 5，显式值钳制到 1~20。
fn num_results(options: &SearchOptions) -> usize {
    options.num_results.map(|n| n.clamp(1, 20)).unwrap_or(5)
}

/// 归一化时间范围，仅接受 day/week/month/year，其余（含未指定）返回 None。
fn recency(options: &SearchOptions) -> Option<&str> {
    options
        .recency
        .as_deref()
        .filter(|r| matches!(*r, "day" | "week" | "month" | "year"))
}

/// 领域过滤归一化：返回 (include, exclude)
fn split_domains(domain_filter: &[String]) -> (Vec<String>, Vec<String>) {
    let mut include = Vec::new();
    let mut exclude = Vec::new();
    for raw in domain_filter {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        let (negative, value) = match raw.strip_prefix('-') {
            Some(rest) => (true, rest.trim()),
            None => (false, raw),
        };
        let domain = http::normalize_hostname(value);
        let Some(domain) = domain else { continue };
        let target = if negative { &mut exclude } else { &mut include };
        if !target.contains(&domain) {
            target.push(domain);
        }
    }
    (include, exclude)
}

/// 生成查询后缀：单个 include 用 `site:x`，多个用 `site:x OR site:y`，exclude 逐个追加 `NOT site:x`。
fn domain_query_suffixes(include: &[String], exclude: &[String]) -> String {
    let mut parts = Vec::new();
    if include.len() == 1 {
        parts.push(format!("site:{}", include[0]));
    } else if include.len() > 1 {
        parts.push(
            include
                .iter()
                .map(|d| format!("site:{d}"))
                .collect::<Vec<_>>()
                .join(" OR "),
        );
    }
    for domain in exclude {
        parts.push(format!("NOT site:{domain}"));
    }
    parts.join(" ")
}

/// Tavily `/search`：取聚合答案与结果，支持时间范围与域名 include/exclude；缺 key 或响应非 2xx 时报错。
async fn tavily(
    client: &reqwest::Client,
    cfg: &Config,
    query: &str,
    options: &SearchOptions,
) -> Result<SearchResponse, ToolError> {
    let key = config::resolve_credential(cfg.tavily_api_key.as_deref(), "TAVILY_API_KEY")
        .ok_or_else(|| ToolError("Tavily API key not found".to_string()))?;
    let url = format!(
        "{}/search",
        http::resolve_base_url(
            cfg.tavily_base_url.as_deref(),
            "TAVILY_BASE_URL",
            "https://api.tavily.com"
        )
    );
    let (include, exclude) = split_domains(&options.domain_filter);
    let mut body = serde_json::json!({
        "query": query,
        "search_depth": "basic",
        "max_results": num_results(options),
        "include_answer": "basic",
        "include_raw_content": false,
    });
    if let Some(range) = recency(options) {
        body["time_range"] = serde_json::json!(range);
    }
    if !include.is_empty() {
        body["include_domains"] = serde_json::json!(include);
    }
    if !exclude.is_empty() {
        body["exclude_domains"] = serde_json::json!(exclude);
    }
    let response = client
        .post(&url)
        .bearer_auth(key)
        .json(&body)
        .send()
        .await
        .map_err(|err| ToolError(format!("Tavily request failed: {err}")))?;
    if !response.status().is_success() {
        return Err(ToolError(format!(
            "Tavily API error {}",
            http::error_summary(response).await
        )));
    }
    let data: serde_json::Value = response
        .json()
        .await
        .map_err(|err| ToolError(format!("Tavily returned invalid JSON: {err}")))?;
    let answer = data
        .get("answer")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let results = map_generic_results(
        data.get("results"),
        num_results(options),
        "title",
        "url",
        "content",
    );
    Ok(SearchResponse::new("tavily", answer, results))
}

/// Brave `/res/v1/web/search`：域名过滤经查询后缀实现，无聚合答案，由结果自行拼 `build_answer`。
async fn brave(
    client: &reqwest::Client,
    cfg: &Config,
    query: &str,
    options: &SearchOptions,
) -> Result<SearchResponse, ToolError> {
    let key = config::resolve_credential(cfg.brave_api_key.as_deref(), "BRAVE_API_KEY")
        .ok_or_else(|| ToolError("Brave Search API key not found".to_string()))?;
    let base = http::resolve_base_url(
        cfg.brave_base_url.as_deref(),
        "BRAVE_BASE_URL",
        "https://api.search.brave.com/res/v1",
    );
    let (include, exclude) = split_domains(&options.domain_filter);
    let suffix = domain_query_suffixes(&include, &exclude);
    let search_query = if suffix.is_empty() {
        query.to_string()
    } else {
        format!("{query} {suffix}")
    };
    let count = if options.domain_filter.is_empty() {
        num_results(options)
    } else {
        20
    };
    let mut params: Vec<(&str, String)> =
        vec![("q", search_query.clone()), ("count", count.to_string())];
    if let Some(range) = recency(options) {
        let freshness = match range {
            "day" => "pd",
            "week" => "pw",
            "month" => "pm",
            "year" => "py",
            _ => "",
        };
        if !freshness.is_empty() {
            params.push(("freshness", freshness.to_string()));
        }
    }
    let borrowed: Vec<(&str, &str)> = params.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let response = client
        .get(http::with_query(&format!("{base}/web/search"), &borrowed))
        .header("X-Subscription-Token", key)
        .header("accept", "application/json")
        .send()
        .await
        .map_err(|err| ToolError(format!("Brave request failed: {err}")))?;
    if !response.status().is_success() {
        return Err(ToolError(format!(
            "Brave API error {}",
            http::error_summary(response).await
        )));
    }
    let data: serde_json::Value = response
        .json()
        .await
        .map_err(|err| ToolError(format!("Brave returned invalid JSON: {err}")))?;
    let mut results = Vec::new();
    if let Some(items) = data.pointer("/web/results").and_then(|v| v.as_array()) {
        for item in items {
            let url = item.get("url").and_then(|v| v.as_str()).unwrap_or("");
            if url.is_empty() || !http::passes_domain_filter(url, &include, &exclude) {
                continue;
            }
            let title = item
                .get("title")
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(url);
            let snippet = item
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            results.push(SearchResult {
                title: title.to_string(),
                url: url.to_string(),
                snippet: snippet.to_string(),
            });
            if results.len() >= num_results(options) {
                break;
            }
        }
    }
    let answer = build_answer(&results);
    Ok(SearchResponse::new("brave", answer, results))
}

/// Exa：无域名/时间/条数约束时走 `/answer` 只取答案，否则走 `/search`（域名、时间范围、条数）；
/// 时间范围换算为起始发布日期。
async fn exa(
    client: &reqwest::Client,
    cfg: &Config,
    query: &str,
    options: &SearchOptions,
) -> Result<SearchResponse, ToolError> {
    let key = config::resolve_credential(cfg.exa_api_key.as_deref(), "EXA_API_KEY")
        .ok_or_else(|| ToolError("Exa API key not found".to_string()))?;
    let base = http::resolve_base_url(
        cfg.exa_base_url.as_deref(),
        "EXA_BASE_URL",
        "https://api.exa.ai",
    );
    let (include, exclude) = split_domains(&options.domain_filter);
    let use_search = !options.domain_filter.is_empty()
        || recency(options).is_some()
        || options.num_results.map(|n| n != 5).unwrap_or(false);

    if !use_search {
        let response = client
            .post(format!("{base}/answer"))
            .header("x-api-key", key)
            .json(&serde_json::json!({ "query": query }))
            .send()
            .await
            .map_err(|err| ToolError(format!("Exa request failed: {err}")))?;
        if !response.status().is_success() {
            return Err(ToolError(format!(
                "Exa API error {}",
                http::error_summary(response).await
            )));
        }
        let data: serde_json::Value = response
            .json()
            .await
            .map_err(|err| ToolError(format!("Exa returned invalid JSON: {err}")))?;
        let answer = data
            .get("answer")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let results = map_generic_results(data.get("citations"), 20, "title", "url", "");
        return Ok(SearchResponse::new("exa", answer, results));
    }

    let mut body = serde_json::json!({
        "query": query,
        "type": "auto",
        "numResults": num_results(options),
        "contents": { "highlights": true },
    });
    if !include.is_empty() {
        body["includeDomains"] = serde_json::json!(include);
    }
    if !exclude.is_empty() {
        body["excludeDomains"] = serde_json::json!(exclude);
    }
    if let Some(range) = recency(options) {
        let days = match range {
            "day" => 1,
            "week" => 7,
            "month" => 30,
            _ => 365,
        };
        let start = crate::utils::time::rfc3339_days_ago(days);
        body["startPublishedDate"] = serde_json::json!(start);
    }
    let response = client
        .post(format!("{base}/search"))
        .header("x-api-key", key)
        .json(&body)
        .send()
        .await
        .map_err(|err| ToolError(format!("Exa request failed: {err}")))?;
    if !response.status().is_success() {
        return Err(ToolError(format!(
            "Exa API error {}",
            http::error_summary(response).await
        )));
    }
    let data: serde_json::Value = response
        .json()
        .await
        .map_err(|err| ToolError(format!("Exa returned invalid JSON: {err}")))?;
    let mut results = Vec::new();
    let mut answer_parts = Vec::new();
    if let Some(items) = data.get("results").and_then(|v| v.as_array()) {
        for item in items {
            let url = item.get("url").and_then(|v| v.as_str()).unwrap_or("");
            if url.is_empty() {
                continue;
            }
            let title = item
                .get("title")
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("Source {}", results.len() + 1));
            let content = item
                .get("highlights")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .filter(|s| !s.trim().is_empty())
                .or_else(|| {
                    item.get("text")
                        .and_then(|v| v.as_str())
                        .map(|s| s.chars().take(1000).collect())
                })
                .unwrap_or_default();
            if !content.trim().is_empty() {
                answer_parts.push(format!("{content}\nSource: {title} ({url})"));
            }
            results.push(SearchResult {
                title,
                url: url.to_string(),
                snippet: String::new(),
            });
            if results.len() >= num_results(options) {
                break;
            }
        }
    }
    Ok(SearchResponse::new(
        "exa",
        answer_parts.join("\n\n"),
        results,
    ))
}

/// Jina Reader Search：把排除域名/时间范围拼进查询串，经 `s.jina.ai` 取 JSON，
/// 再按域名过滤与 URL 去重。
async fn jina(
    client: &reqwest::Client,
    cfg: &Config,
    query: &str,
    options: &SearchOptions,
) -> Result<SearchResponse, ToolError> {
    let key = config::resolve_credential(cfg.jina_api_key.as_deref(), "JINA_API_KEY")
        .ok_or_else(|| ToolError("Jina Search API key not found".to_string()))?;
    let (include, exclude) = split_domains(&options.domain_filter);
    let exclusions = exclude
        .iter()
        .map(|d| format!(" -site:{d}"))
        .collect::<String>();
    let recency_suffix = recency(options)
        .map(|r| format!(" published in the past {r}"))
        .unwrap_or_default();
    let constrained = format!("{}{exclusions}{recency_suffix}", query.trim());
    let mut url = url::Url::parse(&format!(
        "https://s.jina.ai/{}",
        http::urlencode(&constrained)
    ))
    .map_err(|err| ToolError(format!("Invalid Jina URL: {err}")))?;
    url.query_pairs_mut()
        .append_pair("count", &num_results(options).to_string());
    for domain in &include {
        url.query_pairs_mut().append_pair("site", domain);
    }
    let response = client
        .get(url)
        .header("accept", "application/json")
        .bearer_auth(key)
        .header("x-respond-with", "no-content")
        .header("x-retain-images", "none")
        .send()
        .await
        .map_err(|err| ToolError(format!("Jina request failed: {err}")))?;
    if !response.status().is_success() {
        return Err(ToolError(format!(
            "Jina API error {}",
            http::error_summary(response).await
        )));
    }
    let data: serde_json::Value = response
        .json()
        .await
        .map_err(|err| ToolError(format!("Jina returned invalid JSON: {err}")))?;
    let items = match data.get("data") {
        Some(serde_json::Value::Array(items)) => items.clone(),
        _ => {
            return Err(ToolError(
                "Jina Search API returned invalid response: expected data array".to_string(),
            ));
        }
    };
    let mut results = Vec::new();
    let mut seen = HashSet::new();
    for item in &items {
        let url = item
            .get("url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        if url.is_empty() || !seen.insert(url.to_string()) {
            continue;
        }
        if !http::passes_domain_filter(url, &include, &exclude) {
            continue;
        }
        let title = item
            .get("title")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| format!("Source {}", results.len() + 1));
        let snippet = item
            .get("description")
            .and_then(|v| v.as_str())
            .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))
            .unwrap_or_default();
        results.push(SearchResult {
            title,
            url: url.to_string(),
            snippet,
        });
        if results.len() >= num_results(options) {
            break;
        }
    }
    let answer = build_answer(&results);
    Ok(SearchResponse::new("jina", answer, results))
}

/// 自建 SearXNG 的 `/search?format=json`：拼接域名后缀、时间范围并带上配置的附加请求头；
/// 未配置 base URL 时报错，答案取 `answers` 字段再并入结果摘要。
async fn searxng(
    client: &reqwest::Client,
    cfg: &Config,
    query: &str,
    options: &SearchOptions,
) -> Result<SearchResponse, ToolError> {
    let base = cfg
        .searxng_base_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ToolError("SearXNG base URL not configured".to_string()))?
        .trim_end_matches('/')
        .to_string();
    let (include, exclude) = split_domains(&options.domain_filter);
    let suffix = domain_query_suffixes(&include, &exclude);
    let search_query = if suffix.is_empty() {
        query.to_string()
    } else {
        format!("{query} {suffix}")
    };
    let mut params: Vec<(&str, String)> =
        vec![("q", search_query.clone()), ("format", "json".to_string())];
    if let Some(range) = recency(options) {
        params.push(("time_range", range.to_string()));
    }
    let borrowed: Vec<(&str, &str)> = params.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let mut request = client.get(http::with_query(&format!("{base}/search"), &borrowed));
    if let Some(headers) = &cfg.searxng_headers {
        for (name, value) in headers {
            request = request.header(name, value);
        }
    }
    let response = request
        .send()
        .await
        .map_err(|err| ToolError(format!("SearXNG request failed: {err}")))?;
    if !response.status().is_success() {
        return Err(ToolError(format!(
            "SearXNG error {}",
            http::error_summary(response).await
        )));
    }
    let data: serde_json::Value = response
        .json()
        .await
        .map_err(|err| ToolError(format!("SearXNG returned invalid JSON: {err}")))?;
    let mut results = Vec::new();
    if let Some(items) = data.get("results").and_then(|v| v.as_array()) {
        for item in items {
            let url = item.get("url").and_then(|v| v.as_str()).unwrap_or("");
            if url.is_empty() || !http::passes_domain_filter(url, &include, &exclude) {
                continue;
            }
            let title = item
                .get("title")
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(url);
            let snippet = item.get("content").and_then(|v| v.as_str()).unwrap_or("");
            results.push(SearchResult {
                title: title.to_string(),
                url: url.to_string(),
                snippet: snippet.to_string(),
            });
            if results.len() >= num_results(options) {
                break;
            }
        }
    }
    let mut answer_parts: Vec<String> = data
        .get("answers")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    answer_parts.push(build_answer(&results));
    let answer = answer_parts
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    Ok(SearchResponse::new("searxng", answer, results))
}

/// DuckDuckGo HTML 端（免密钥）：抓取 HTML 后解析结果，解析为空视为无效响应并报错。
async fn duckduckgo(
    client: &reqwest::Client,
    query: &str,
    options: &SearchOptions,
) -> Result<SearchResponse, ToolError> {
    let response = client
        .get(http::with_query(
            "https://html.duckduckgo.com/html/",
            &[("q", query)],
        ))
        .header("accept", "text/html")
        .send()
        .await
        .map_err(|err| ToolError(format!("DuckDuckGo request failed: {err}")))?;
    if !response.status().is_success() {
        return Err(ToolError(format!(
            "DuckDuckGo search error {}",
            http::error_summary(response).await
        )));
    }
    let html = response
        .text()
        .await
        .map_err(|err| ToolError(format!("DuckDuckGo body read failed: {err}")))?;
    let parsed = parse_duckduckgo_html(&html);
    if parsed.is_empty() {
        return Err(ToolError(
            "DuckDuckGo returned no parseable results (invalid response)".to_string(),
        ));
    }
    let (include, exclude) = split_domains(&options.domain_filter);
    let mut results = Vec::new();
    for item in parsed {
        if !http::passes_domain_filter(&item.url, &include, &exclude) {
            continue;
        }
        results.push(item);
        if results.len() >= num_results(options) {
            break;
        }
    }
    let answer = build_answer(&results);
    Ok(SearchResponse::new("duckduckgo", answer, results))
}

/// 从 DuckDuckGo HTML 里抽取 `result__a` 链接与 `result__snippet` 摘要，
/// 剥标签、解码实体并压缩空白；标题为空或链接非 http(s) 的条目丢弃。
fn parse_duckduckgo_html(html: &str) -> Vec<SearchResult> {
    /// 结果链接（`result__a`）正则缓存
    static ANCHOR: OnceLock<Regex> = OnceLock::new();
    /// 结果摘要（`result__snippet`）正则缓存
    static SNIPPET: OnceLock<Regex> = OnceLock::new();
    /// HTML 标签剥离正则缓存
    static TAGS: OnceLock<Regex> = OnceLock::new();

    let anchor = ANCHOR.get_or_init(|| {
        Regex::new(r#"(?is)<a\b[^>]*class="[^"]*result__a[^"]*"[^>]*href="([^"]+)"[^>]*>(.*?)</a>"#)
            .expect("ddg anchor regex")
    });
    let snippet = SNIPPET.get_or_init(|| {
        Regex::new(r#"(?is)<a\b[^>]*class="[^"]*result__snippet[^"]*"[^>]*>(.*?)</a>"#)
            .expect("ddg snippet regex")
    });
    let tags = TAGS.get_or_init(|| Regex::new(r"(?is)<[^>]+>").expect("tag regex"));

    let snippets: Vec<String> = snippet
        .captures_iter(html)
        .map(|caps| {
            let raw = tags.replace_all(&caps[1], "");
            collapse_ws(&super::fetch::decode_entities(&raw))
        })
        .collect();

    let mut results = Vec::new();
    for (index, caps) in anchor.captures_iter(html).enumerate() {
        let href = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        let title_raw = tags.replace_all(&caps[2], "");
        let title = collapse_ws(&super::fetch::decode_entities(&title_raw));
        let Some(url) = decode_duckduckgo_url(href) else {
            continue;
        };
        if title.is_empty() {
            continue;
        }
        results.push(SearchResult {
            title,
            url,
            snippet: snippets.get(index).cloned().unwrap_or_default(),
        });
    }
    results
}

/// 把任意空白序列压缩为单个空格并去掉首尾空白。
fn collapse_ws(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 解出 DuckDuckGo 跳转链接的真实目标（`uddg` 参数）；非 http(s) 或解析失败返回 None。
fn decode_duckduckgo_url(href: &str) -> Option<String> {
    let base = url::Url::parse("https://html.duckduckgo.com/html/").ok()?;
    let link = base.join(href).ok()?;
    let destination = link
        .query_pairs()
        .find(|(k, _)| k == "uddg")
        .map(|(_, v)| v.into_owned())
        .unwrap_or_else(|| link.to_string());
    let parsed = url::Url::parse(&destination).ok()?;
    match parsed.scheme() {
        "http" | "https" => Some(parsed.to_string()),
        _ => None,
    }
}

/// Serper（Google）`/search`：域名后缀拼进查询，有域名过滤时多取 5 条以补偿被过滤项；
/// 时间范围映射为 `tbs`（qdr:d/w/m/y）。
async fn serper(
    client: &reqwest::Client,
    cfg: &Config,
    query: &str,
    options: &SearchOptions,
) -> Result<SearchResponse, ToolError> {
    let key = config::resolve_credential(cfg.serper_api_key.as_deref(), "SERPER_API_KEY")
        .ok_or_else(|| ToolError("Serper API key not found".to_string()))?;
    let (include, exclude) = split_domains(&options.domain_filter);
    let suffix = domain_query_suffixes(&include, &exclude);
    let search_query = if suffix.is_empty() {
        query.to_string()
    } else {
        format!("{query} {suffix}")
    };
    let count = if options.domain_filter.is_empty() {
        num_results(options)
    } else {
        (num_results(options) + 5).min(20)
    };
    let mut body = serde_json::json!({ "q": search_query, "num": count });
    if let Some(range) = recency(options) {
        let tbs = match range {
            "day" => "qdr:d",
            "week" => "qdr:w",
            "month" => "qdr:m",
            _ => "qdr:y",
        };
        body["tbs"] = serde_json::json!(tbs);
    }
    let response = client
        .post("https://google.serper.dev/search")
        .header("X-API-KEY", key)
        .json(&body)
        .send()
        .await
        .map_err(|err| ToolError(format!("Serper request failed: {err}")))?;
    if !response.status().is_success() {
        return Err(ToolError(format!(
            "Serper API error {}",
            http::error_summary(response).await
        )));
    }
    let data: serde_json::Value = response
        .json()
        .await
        .map_err(|err| ToolError(format!("Serper returned invalid JSON: {err}")))?;
    let mut results = Vec::new();
    if let Some(entries) = data.get("organic").and_then(|v| v.as_array()) {
        for entry in entries {
            let link = entry.get("link").and_then(|v| v.as_str()).unwrap_or("");
            if link.is_empty() || !http::passes_domain_filter(link, &include, &exclude) {
                continue;
            }
            let title = entry
                .get("title")
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("Source {}", results.len() + 1));
            let snippet = entry.get("snippet").and_then(|v| v.as_str()).unwrap_or("");
            results.push(SearchResult {
                title,
                url: link.to_string(),
                snippet: snippet.to_string(),
            });
            if results.len() >= num_results(options) {
                break;
            }
        }
    }
    let answer = build_answer(&results);
    Ok(SearchResponse::new("serper", answer, results))
}

/// Perplexity `/chat/completions`（sonar）：取 `choices[0].message.content` 为答案，
/// `citations` 映射为结果；域名过滤用 `search_domain_filter`（排除项加 `-` 前缀）。
async fn perplexity(
    client: &reqwest::Client,
    cfg: &Config,
    query: &str,
    options: &SearchOptions,
) -> Result<SearchResponse, ToolError> {
    let key = config::resolve_credential(cfg.perplexity_api_key.as_deref(), "PERPLEXITY_API_KEY")
        .ok_or_else(|| ToolError("Perplexity API key not found".to_string()))?;
    let (include, exclude) = split_domains(&options.domain_filter);
    let mut body = serde_json::json!({
        "model": "sonar",
        "messages": [{ "role": "user", "content": query }],
        "max_tokens": 1024,
        "return_related_questions": false,
    });
    if let Some(range) = recency(options) {
        body["search_recency_filter"] = serde_json::json!(range);
    }
    let mut domains = include.clone();
    domains.extend(exclude.iter().map(|d| format!("-{d}")));
    if !domains.is_empty() {
        body["search_domain_filter"] = serde_json::json!(domains);
    }
    let response = client
        .post("https://api.perplexity.ai/chat/completions")
        .bearer_auth(key)
        .json(&body)
        .send()
        .await
        .map_err(|err| ToolError(format!("Perplexity request failed: {err}")))?;
    if !response.status().is_success() {
        return Err(ToolError(format!(
            "Perplexity API error {}",
            http::error_summary(response).await
        )));
    }
    let data: serde_json::Value = response
        .json()
        .await
        .map_err(|err| ToolError(format!("Perplexity returned invalid JSON: {err}")))?;
    let answer = data
        .pointer("/choices/0/message/content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let results = map_generic_results(data.get("citations"), 20, "title", "url", "");
    Ok(SearchResponse::new("perplexity", answer, results))
}

/// 把 `[{title,url,snippet}]` 结构映射为统一结果。
fn map_generic_results(
    value: Option<&Value>,
    limit: usize,
    title_key: &str,
    url_key: &str,
    snippet_key: &str,
) -> Vec<SearchResult> {
    let Some(items) = value.and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let mut results = Vec::new();
    for item in items {
        // citations 可能是纯字符串数组
        if let Some(url) = item.as_str() {
            results.push(SearchResult {
                title: format!("Source {}", results.len() + 1),
                url: url.to_string(),
                snippet: String::new(),
            });
            if results.len() >= limit {
                break;
            }
            continue;
        }
        let url = item.get(url_key).and_then(|v| v.as_str()).unwrap_or("");
        if url.is_empty() {
            continue;
        }
        let title = item
            .get(title_key)
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("Source {}", results.len() + 1));
        let snippet = if snippet_key.is_empty() {
            String::new()
        } else {
            item.get(snippet_key)
                .and_then(|v| v.as_str())
                .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))
                .unwrap_or_default()
        };
        results.push(SearchResult {
            title,
            url: url.to_string(),
            snippet,
        });
        if results.len() >= limit {
            break;
        }
    }
    results
}

/// 无 answer 的 provider 用「snippet + Source:」拼出 answer。
fn build_answer(results: &[SearchResult]) -> String {
    results
        .iter()
        .map(|r| {
            if r.snippet.is_empty() {
                format!("Source: {} ({})", r.title, r.url)
            } else {
                format!("{}\nSource: {} ({})", r.snippet, r.title, r.url)
            }
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::web_access::config::LoadedConfig;

    #[test]
    fn parses_provider_selection() {
        assert_eq!(ProviderSelection::from_json(None), ProviderSelection::Auto);
        assert_eq!(
            ProviderSelection::from_json(Some(&serde_json::json!("auto"))),
            ProviderSelection::Auto
        );
        assert_eq!(
            ProviderSelection::from_json(Some(&serde_json::json!("all"))),
            ProviderSelection::All
        );
        assert_eq!(
            ProviderSelection::from_json(Some(&serde_json::json!("Brave"))),
            ProviderSelection::Single("Brave".to_string())
        );
        assert_eq!(
            ProviderSelection::from_json(Some(&serde_json::json!(["brave", "tavily"]))),
            ProviderSelection::List(vec!["brave".to_string(), "tavily".to_string()])
        );
        assert_eq!(
            ProviderSelection::from_json(Some(&serde_json::json!([]))),
            ProviderSelection::Auto
        );
    }

    #[test]
    fn provider_strum_conversions() {
        // 名称 → 变体（去空白、大小写不敏感、支持别名）
        assert_eq!(Provider::parse("  DDG "), Some(Provider::DuckDuckGo));
        assert_eq!(Provider::parse("duckduckgo"), Some(Provider::DuckDuckGo));
        assert_eq!(Provider::parse("Perplexity"), Some(Provider::Perplexity));
        assert_eq!(Provider::parse("SEARXNG"), Some(Provider::Searxng));
        assert_eq!(Provider::parse("nope"), None);

        // 变体 → 规范名称（别名不改变规范名）
        assert_eq!(Provider::DuckDuckGo.name(), "duckduckgo");
        assert_eq!(Provider::Jina.name(), "jina");

        // 判别值即偏好顺序，`iter()` 按声明顺序遍历且覆盖全部成员
        let ranks: Vec<usize> = Provider::iter().map(|p| p.preference_rank()).collect();
        assert_eq!(ranks, (0..8).collect::<Vec<_>>());

        // `ALL` 已移除，`iter()` 是唯一来源
        assert_eq!(Provider::iter().count(), 8);
    }

    #[test]
    fn domain_filter_helpers() {
        let (inc, exc) = split_domains(&[
            "github.com".to_string(),
            "-old.example.com".to_string(),
            "https://docs.rs/crate".to_string(),
        ]);
        assert_eq!(inc, vec!["github.com", "docs.rs"]);
        assert_eq!(exc, vec!["old.example.com"]);
        assert!(http::passes_domain_filter(
            "https://github.com/a/b",
            &inc,
            &exc
        ));
        assert!(!http::passes_domain_filter(
            "https://old.example.com/x",
            &inc,
            &exc
        ));
        assert!(!http::passes_domain_filter(
            "https://other.com/x",
            &inc,
            &exc
        ));
    }

    #[test]
    fn duckduckgo_url_decoding() {
        let href = "//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fpage&rut=abc";
        assert_eq!(
            decode_duckduckgo_url(href),
            Some("https://example.com/page".to_string())
        );
        assert_eq!(decode_duckduckgo_url("javascript:void(0)"), None);
    }

    #[test]
    fn duckduckgo_html_parsing() {
        let html = r#"
          <div class="result"><a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fone">Example <b>One</b></a>
          <a class="result__snippet">Snippet &amp; one</a></div>
          <div class="result result--ad"><a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fads.example%2F">Ad</a></div>
        "#;
        let results = parse_duckduckgo_html(html);
        assert_eq!(
            results.len(),
            2,
            "ad 也被解析（上游按 class 过滤，解析层不做广告判别）"
        );
        assert_eq!(results[0].title, "Example One");
        assert_eq!(results[0].url, "https://example.com/one");
        assert_eq!(results[0].snippet, "Snippet & one");
    }

    #[test]
    fn availability_and_routing() {
        // 显式只配 Brave/SearxNG 时，Tavily 必须判为不可用 —— 而 `is_available` 会回退读
        // 进程环境变量，故先清空全部搜索凭据（drop 时自动还原）。
        let _env = crate::test_support::clear_search_credential_env();
        let loaded = LoadedConfig {
            config: Config {
                brave_api_key: Some("k".to_string()),
                searxng_base_url: Some("https://search.local".to_string()),
                ..Config::default()
            },
            error: None,
        };
        let available = available_providers(&loaded.config);
        assert!(available.contains(&Provider::Brave));
        assert!(available.contains(&Provider::Searxng));
        assert!(available.contains(&Provider::DuckDuckGo));
        assert!(!available.contains(&Provider::Tavily));
        // auto 路由：SearXNG 优先于 Brave
        let candidates = resolve_candidates(&ProviderSelection::Auto, &loaded).unwrap();
        assert_eq!(candidates[0], Provider::Searxng);
        assert!(candidates.contains(&Provider::Brave));
        // all 排除显式专用 DuckDuckGo
        let all = resolve_candidates(&ProviderSelection::All, &loaded).unwrap();
        assert!(!all.contains(&Provider::DuckDuckGo));
        assert!(all.contains(&Provider::Searxng));
        // 未配置 provider 报错
        assert!(resolve_candidates(&ProviderSelection::Single("tavily".into()), &loaded).is_err());
    }

    #[test]
    fn auto_routing_falls_back_to_duckduckgo_when_nothing_configured() {
        // 需密钥的 provider 都依赖进程级环境变量，故清空后串行断言（drop 时还原）
        let _env = crate::test_support::clear_search_credential_env();

        let loaded = LoadedConfig {
            config: Config::default(),
            error: None,
        };
        // 其它 provider 全不可用 → 兜底到免密钥的 DuckDuckGo
        assert_eq!(
            available_providers(&loaded.config),
            vec![Provider::DuckDuckGo]
        );
        assert_eq!(
            resolve_candidates(&ProviderSelection::Auto, &loaded).unwrap(),
            vec![Provider::DuckDuckGo]
        );
    }

    #[test]
    fn urlencoding_helper() {
        // Jina 把查询编进路径，空格必须是 %20
        assert_eq!(http::urlencode("a b/c"), "a%20b%2Fc");
    }
}
