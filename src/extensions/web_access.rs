//! `web-access` 扩展（移植自 pi-web-access v0.29.0）。
//!
//! 提供联网能力：
//! - `web_search`：多 provider 聚合搜索（tavily/brave/exa/jina/searxng/duckduckgo/serper/perplexity）；
//! - `fetch_content`：URL 抓取（`readable` HTML→markdown / `raw` 原文），带 SSRF 防护；
//! - `get_search_content`：按 `responseId` 取回搜索结果或抓取正文的切片，支持 `findText` 检索。
//!
//! 配置在 `agent_dir()/extensions/web-search.json`（键名与上游一致，见 [`config`]）。

mod config;
mod fetch;
mod search;
mod store;

use super::util::default_user_agent;
use crate::{
    core::{
        extensions::{
            Extension, ExtensionMode, ExtensionTool, ForkProjectInfo, ToolAnnotations, ToolExecCtx,
            ToolExposure,
        },
        provider::AgentMessage,
        tools::{ToolError, ToolResult},
    },
    extensions::{EXTENSION_FACTORIES, ExtensionFactory, PRIORITY_WEB_ACCESS},
    utils::find::{FindMode, FindResult, find_content},
};
use config::{FeatureTool, LoadedConfig, max_inline_content_chars};
use fetch::{FetchMode, FetchOptions, FetchedContent};
use futures_util::{StreamExt, future::BoxFuture};
use search::{ProviderSelection, SearchOptions, SearchResult};
use serde_json::{Value, json};
use std::sync::Arc;
use store::{QueryResultData, StoredData};

/// 扩展名
const EXT: &str = "web-access";
/// 工具名
const TOOL_WEB_SEARCH: &str = "web_search";
/// `fetch_content` 工具名，用于抓取 URL 正文。
const TOOL_FETCH_CONTENT: &str = "fetch_content";
/// `get_search_content` 工具名，用于按 responseId 回读取回内容切片。
const TOOL_GET_SEARCH_CONTENT: &str = "get_search_content";

/// `includeContent` 同步抓取的 URL 上限（避免一次工具调用过长）
const INCLUDE_CONTENT_FETCH_LIMIT: usize = 10;

/// 内容检索（[`crate::utils::find::find_content`]）的上下文窗口：
/// 每个命中点前后各取多少个字符作为片段
pub const CONTEXT_CHARS: usize = 400;
/// 内容检索生成文本的总长度上限（字符数），超出则截断
pub const MAX_OUTPUT_CHARS: usize = 20_000;

/// 注册进扩展工厂分布切片的 web-access 扩展构造器。
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static WEB_ACCESS_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_WEB_ACCESS,
    make: || -> Arc<dyn Extension> { Arc::new(WebAccess) },
};

/// web-access 扩展主体
pub struct WebAccess;

impl Extension for WebAccess {
    /// 扩展名（固定 `web-access`，用于工具分发与诊断）。
    fn name(&self) -> &str {
        EXT
    }

    /// 面板展示的一句话描述，说明本扩展提供的三类联网工具。
    fn description(&self) -> &str {
        "Web search, URL fetching, and stored-content retrieval (web_search / fetch_content / get_search_content)."
    }

    /// 声明本扩展移植自 pi-web-access 0.29.0，供 `/extension` 面板展示来源链接。
    fn fork_project(&self) -> Option<ForkProjectInfo> {
        Some(ForkProjectInfo {
            plugin_name: "pi-web-access".to_string(),
            plugin_version: "0.29.0".to_string(),
            url: "https://github.com/nicobailon/pi-web-access".to_string(),
        })
    }

    /// 仅在 Dev 与 Creator 两个并排模式下可用（并排模式需显式列出）。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Dev, ExtensionMode::Creator]
    }

    /// 默认禁用：联网工具会改变 agentic 行为，需用户手动开启。
    fn default_enabled(&self) -> bool {
        false
    }

    /// 按配置文件中各功能开关（web_search / fetch_content / get_search_content）
    /// 构造已启用的工具列表；全部关闭时返回空列表。
    fn tools(&self) -> Vec<ExtensionTool> {
        let loaded = config::load();
        let mut tools = Vec::new();
        if loaded.tool_enabled(FeatureTool::WebSearch) {
            tools.push(web_search_tool());
        }
        if loaded.tool_enabled(FeatureTool::FetchContent) {
            tools.push(fetch_content_tool());
        }
        if loaded.tool_enabled(FeatureTool::GetSearchContent) {
            tools.push(get_search_content_tool(max_inline_content_chars(
                &loaded.config,
            )));
        }
        tools
    }

    /// 启用态变化时：首次启用落盘默认配置模板，随后失效配置缓存，
    /// 使下一次 `tools()` 重新读盘。
    fn on_enabled_changed(&self, enabled: bool) {
        // 首次启用且无配置文件时落盘默认模板，方便用户对照配置。
        if enabled {
            config::ensure_config_file();
        }

        // 启用状态变化后清配置缓存，下一次 tools() 重新读盘。
        config::invalidate();
    }

    /// 会话切换（/new /resume /import /fork /clone）：新会话 = 全新上下文，
    /// 旧的 responseId 不该再取回上一个会话的内容，直接清空存储。
    fn on_session_switched(&self, _session_path: Option<&str>, _messages: &[AgentMessage]) {
        store::clear();
    }

    /// 按工具名分发到对应的执行函数（搜索 / 抓取 / 回读）；
    /// 未知工具名返回 `ToolError`。
    fn execute_tool_async(
        &self,
        name: String,
        args: Value,
        ctx: ToolExecCtx,
    ) -> BoxFuture<'static, Result<ToolResult, ToolError>> {
        Box::pin(async move {
            match name.as_str() {
                TOOL_WEB_SEARCH => run_web_search(&args).await,
                TOOL_FETCH_CONTENT => run_fetch_content(&args, &ctx).await,
                TOOL_GET_SEARCH_CONTENT => run_get_search_content(&args),
                other => Err(ToolError(format!("extension {EXT}: unknown tool: {other}"))),
            }
        })
    }
}

/// 构造 `web_search` 工具定义（描述、参数 JSON Schema 与 snippet）。
fn web_search_tool() -> ExtensionTool {
    ExtensionTool {
        exposure: ToolExposure::Direct,
        namespace: None,
        annotations: ToolAnnotations::default(),
        output_schema: None,
        name: TOOL_WEB_SEARCH.to_string(),
        description: "Search the web via Tavily, Brave, Exa, Jina, SearXNG, DuckDuckGo, Serper, or Perplexity. Returns a synthesized answer with source citations. Pass a provider array to search only those providers simultaneously, use provider \"all\" to search every eligible provider, or omit provider to use the configured default. Prefer `queries` with 2-4 varied angles over a single query for broader coverage. With includeContent, full page content is fetched and stored under a responseId retrievable via get_search_content.".to_string(),
        label: Some("Web Search".to_string()),
        parameters: json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Single search query. For research tasks, prefer 'queries' with multiple varied angles instead."
                },
                "queries": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Multiple queries searched concurrently (up to three at a time), each returning its own synthesized answer."
                },
                "numResults": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 20,
                    "description": "Results per query (default: 5, max: 20)"
                },
                "includeContent": {
                    "type": "boolean",
                    "description": "Fetch full page content for result URLs and store it for get_search_content."
                },
                "recencyFilter": {
                    "type": "string",
                    "enum": ["day", "week", "month", "year"],
                    "description": "Filter by recency"
                },
                "domainFilter": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Limit to domains (prefix with - to exclude)"
                },
                "provider": {
                    "anyOf": [
                        { "type": "string" },
                        { "type": "array", "items": { "type": "string" } }
                    ],
                    "description": "Search provider, list of providers to search simultaneously, or \"all\"; omit to use the configured provider, or \"auto\"."
                },
                "proxy": {
                    "type": "string",
                    "description": "http(s) or socks proxy URL used for every outbound request in this call. Empty string forces direct access."
                }
            }
        }),
        snippet: "Use for web research questions. Prefer {queries:[...]} with 2-4 varied angles over a single query. Omit provider unless overriding the configured default.".to_string(),
        prompt_guidelines: Vec::new(),
        constrained_sampling: false,
        render_shell: None,
        execution_mode: None,
        prepare_arguments: None,
        grammar_sampling: None,
    }
}

/// 构造 `fetch_content` 工具定义（描述、参数 JSON Schema 与 snippet）。
fn fetch_content_tool() -> ExtensionTool {
    ExtensionTool {
        exposure: ToolExposure::Direct,
        namespace: None,
        annotations: ToolAnnotations::default(),
        output_schema: None,
        name: TOOL_FETCH_CONTENT.to_string(),
        description: "Fetch URL(s) and extract readable content as markdown. Use mode \"raw\" for exact textual HTTP response bodies. Content is sliced to maxInlineContentChars and stored under a responseId for get_search_content.".to_string(),
        label: Some("Fetch Content".to_string()),
        parameters: json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "Single URL to fetch" },
                "urls": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Multiple URLs (parallel)"
                },
                "mode": {
                    "type": "string",
                    "enum": ["readable", "raw"],
                    "description": "readable (default extraction) or raw (exact textual HTTP body)"
                },
                "proxy": {
                    "type": "string",
                    "description": "http(s) or socks proxy URL used for this fetch. Empty string forces direct access."
                }
            }
        }),
        snippet: "Use to fetch readable or raw URL content into markdown/text.".to_string(),
        prompt_guidelines: Vec::new(),
        constrained_sampling: false,
        render_shell: None,
        execution_mode: None,
        prepare_arguments: None,
        grammar_sampling: None,
    }
}

/// 构造 `get_search_content` 工具定义；`max_inline` 决定 `limit` 参数的取值上限。
fn get_search_content_tool(max_inline: usize) -> ExtensionTool {
    ExtensionTool {
        exposure: ToolExposure::Direct,
        namespace: None,
        annotations: ToolAnnotations::default(),
        output_schema: None,
        name: TOOL_GET_SEARCH_CONTENT.to_string(),
        description: "Retrieve bounded content slices or find matching passages in a previous web_search or fetch_content call.".to_string(),
        label: Some("Get Search Content".to_string()),
        parameters: json!({
            "type": "object",
            "properties": {
                "responseId": { "type": "string", "description": "The responseId from web_search or fetch_content" },
                "query": { "type": "string", "description": "Get content for this exact search query" },
                "queryIndex": { "type": "integer", "minimum": 0, "description": "Get content for query at index" },
                "url": { "type": "string", "description": "Get content for this URL" },
                "urlIndex": { "type": "integer", "minimum": 0, "description": "Get content for URL at index" },
                "offset": { "type": "integer", "minimum": 0, "description": "Character offset for fetched URL content slices (default 0). Ignored when findText is supplied." },
                "limit": { "type": "integer", "minimum": 1, "maximum": max_inline, "description": "Maximum characters to return for fetched URL content slices. Ignored when findText is supplied." },
                "findText": {
                    "anyOf": [
                        { "type": "string" },
                        { "type": "array", "items": { "type": "string" } }
                    ],
                    "description": "Text or texts to find in the selected stored content. When supplied, offset and limit are ignored."
                },
                "findMode": {
                    "type": "string",
                    "enum": ["exact", "case-insensitive", "fuzzy"],
                    "description": "Matching mode for findText (default: case-insensitive). Requires findText."
                }
            },
            "required": ["responseId"]
        }),
        snippet: "Use after web_search/fetch_content to retrieve stored content via responseId; findText locates passages without paging.".to_string(),
        prompt_guidelines: Vec::new(),
        constrained_sampling: false,
        render_shell: None,
        execution_mode: None,
        prepare_arguments: None,
        grammar_sampling: None,
    }
}

/// 执行 `web_search`：多条查询最多 3 路并发，汇总各查询的答案与来源，
/// 可选 `includeContent` 同步抓取结果页并存入 store；搜索与抓取结果均以
/// responseId 落库。配置加载失败、未提供查询或参数非法时返回 `Err`。
async fn run_web_search(args: &Value) -> Result<ToolResult, ToolError> {
    let loaded = config::load();
    if let Some(error) = &loaded.error {
        return Err(ToolError(error.clone()));
    }

    let mut queries = query_list(args);
    let query_list = normalize_query_list(&queries);
    if query_list.is_empty() {
        return Err(ToolError(
            "Error: No query provided. Use 'query' or 'queries' parameter.".to_string(),
        ));
    }
    queries = query_list;

    let num_results = args
        .get("numResults")
        .and_then(|v| v.as_i64())
        .map(|n| n.clamp(1, 20) as usize);
    let recency = args
        .get("recencyFilter")
        .and_then(|v| v.as_str())
        .filter(|r| matches!(*r, "day" | "week" | "month" | "year"))
        .map(str::to_string);
    let domain_filter: Vec<String> = args
        .get("domainFilter")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let selection = ProviderSelection::from_json(args.get("provider"));
    let proxy = args
        .get("proxy")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let include_content = args
        .get("includeContent")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let options = SearchOptions {
        num_results,
        recency,
        domain_filter,
    };

    // 最多 3 条查询并发
    let mut query_results: Vec<QueryResultData> = Vec::new();
    {
        let owned: Vec<(
            String,
            ProviderSelection,
            SearchOptions,
            LoadedConfig,
            Option<String>,
        )> = queries
            .iter()
            .map(|query| {
                (
                    query.clone(),
                    selection.clone(),
                    options.clone(),
                    loaded.clone(),
                    proxy.clone(),
                )
            })
            .collect();

        let outcomes: Vec<QueryResultData> = futures_util::stream::iter(owned)
            .map(|(query, selection, options, loaded, proxy)| async move {
                match search::search(&query, &selection, &options, &loaded, proxy.as_deref()).await
                {
                    Ok(response) => QueryResultData {
                        query,
                        answer: response.answer,
                        results: response.results,
                        error: None,
                        provider: Some(response.provider),
                    },
                    Err(err) => QueryResultData {
                        query,
                        answer: String::new(),
                        results: Vec::new(),
                        error: Some(err.0),
                        provider: None,
                    },
                }
            })
            .buffered(3)
            .collect()
            .await;
        query_results.extend(outcomes);
    }

    let successful = query_results.iter().filter(|q| q.error.is_none()).count();
    let total_results: usize = query_results.iter().map(|q| q.results.len()).sum();

    let mut output = String::new();
    for result in &query_results {
        if query_results.len() > 1 {
            output.push_str(&format!("## Query: \"{}\"\n\n", result.query));
        }
        match &result.error {
            Some(error) => output.push_str(&format!("Error: {error}\n\n")),
            None => {
                output.push_str(&format_search_summary(&result.results, &result.answer));
                output.push_str("\n\n");
            }
        }
    }

    // includeContent：同步抓取结果页（上限 10），存入 fetch 存储
    let mut fetch_id: Option<String> = None;
    if include_content {
        let mut urls: Vec<String> = Vec::new();
        for result in &query_results {
            for item in &result.results {
                if !urls.contains(&item.url) {
                    urls.push(item.url.clone());
                }
            }
        }

        urls.truncate(INCLUDE_CONTENT_FETCH_LIMIT);
        if !urls.is_empty() {
            let options = fetch_options(&loaded, FetchMode::Readable, proxy.as_deref());
            let fetched = fetch_many(&urls, &options, 3).await;
            let id = store::store_fetch(fetched.clone());
            let ok = fetched.iter().filter(|f| f.error.is_none()).count();
            output.push_str(&format!(
                "---\nFull content for {ok}/{} sources available [{id}].\n",
                fetched.len()
            ));
            fetch_id = Some(id);
        }
    }

    let search_id = store::store_search(query_results.clone());
    output.push_str(&format!(
        "\n---\nResults stored as responseId \"{search_id}\". Use {TOOL_GET_SEARCH_CONTENT}({{ responseId: \"{search_id}\", queryIndex: 0 }}) to retrieve them."
    ));

    let details = json!({
        "queries": queries,
        "queryCount": queries.len(),
        "successfulQueries": successful,
        "totalResults": total_results,
        "includeContent": include_content,
        "fetchId": fetch_id,
        "searchId": search_id,
    });

    Ok(ToolResult {
        text: output.trim().to_string(),
        details: Some(details),
        ..Default::default()
    })
}

/// 把单条查询的答案与来源列表渲染成 markdown（答案 + `**Sources:**` 列表）；
/// 无结果时输出「No results found.」文案。
fn format_search_summary(results: &[SearchResult], answer: &str) -> String {
    if results.is_empty() {
        return if answer.is_empty() {
            "No results found.".to_string()
        } else {
            format!("{answer}\n\n---\n\n**Sources:**\nNo sources returned.")
        };
    }
    let mut output = if answer.is_empty() {
        String::new()
    } else {
        format!("{answer}\n\n---\n\n**Sources:**\n")
    };
    let body = results
        .iter()
        .enumerate()
        .map(|(index, r)| format!("{}. {}\n   {}", index + 1, r.title, r.url))
        .collect::<Vec<_>>()
        .join("\n\n");
    output.push_str(&body);
    output
}

/// 单字符串若是 JSON 字符串数组则展开。
fn query_list(args: &Value) -> Vec<String> {
    if let Some(items) = args.get("queries").and_then(|v| v.as_array()) {
        let list: Vec<String> = items
            .iter()
            .filter_map(|v| v.as_str())
            .map(str::to_string)
            .collect();
        if !list.is_empty() {
            return list;
        }
    }
    match args.get("query").and_then(|v| v.as_str()) {
        Some(query) => expand_query_string(query),
        None => Vec::new(),
    }
}

/// 若字符串是 JSON 字符串数组（如 `["a","b"]`）则展开为多条查询，
/// 否则原样返回单元素列表。
fn expand_query_string(query: &str) -> Vec<String> {
    let trimmed = query.trim();
    if trimmed.starts_with('[')
        && trimmed.ends_with(']')
        && let Ok(serde_json::Value::Array(items)) = serde_json::from_str::<Value>(trimmed)
        && !items.is_empty()
        && items.iter().all(|v| v.is_string())
    {
        return items
            .into_iter()
            .filter_map(|v| v.as_str().map(str::trim).map(str::to_string))
            .filter(|s| !s.is_empty())
            .collect();
    }
    vec![query.to_string()]
}

/// 去除每条查询首尾空白、丢弃空串并对重复项去重，保持原有顺序。
fn normalize_query_list(queries: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for query in queries {
        let trimmed = query.trim();
        if !trimmed.is_empty() && !out.iter().any(|q| q == trimmed) {
            out.push(trimmed.to_string());
        }
    }
    out
}

/// 执行 `fetch_content`：最多 3 路并发抓取 `url`/`urls`，结果存入 store 并返回 responseId。
/// 单 URL 返回正文首片并附续读提示，多 URL 返回概览；`prompt`/`frames`/`auth` 等
/// 未移植参数、非法 mode 或无 URL 时返回 `Err`。
async fn run_fetch_content(args: &Value, ctx: &ToolExecCtx) -> Result<ToolResult, ToolError> {
    let loaded = config::load();
    if let Some(error) = &loaded.error {
        return Err(ToolError(error.clone()));
    }

    // 未移植能力：显式报错，避免静默降级
    for key in [
        "prompt",
        "timestamp",
        "frames",
        "answerModel",
        "forceClone",
        "auth",
    ] {
        if args.get(key).map(|v| !v.is_null()).unwrap_or(false) {
            return Err(ToolError(format!(
                "fetch_content parameter \"{key}\" is not supported yet in this port"
            )));
        }
    }

    let mode = match args.get("mode").and_then(|v| v.as_str()) {
        None => FetchMode::Readable,
        Some(text) => FetchMode::parse(text).ok_or_else(|| {
            ToolError(format!(
                "Error: unknown mode \"{text}\"; use \"readable\" or \"raw\"."
            ))
        })?,
    };

    let url_list = url_list(args);
    if url_list.is_empty() {
        return Err(ToolError(
            "Error: No URL provided. Use 'url' or 'urls' parameter.".to_string(),
        ));
    }

    if mode == FetchMode::Raw {
        for key in ["prompt", "timestamp", "frames"] {
            if args.get(key).map(|v| !v.is_null()).unwrap_or(false) {
                return Err(ToolError(format!(
                    "Error: mode raw cannot be combined with {key}."
                )));
            }
        }
    }

    let proxy = args
        .get("proxy")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| local_proxy_hint(ctx));
    let options = fetch_options(&loaded, mode, proxy.as_deref());

    let fetched = fetch_many(&url_list, &options, 3).await;
    let response_id = store::store_fetch(fetched.clone());
    let max_inline = max_inline_content_chars(&loaded.config);

    if url_list.len() == 1 {
        let result = &fetched[0];
        if let Some(error) = &result.error {
            return Ok(ToolResult {
                text: format!("Error: {error}"),
                details: Some(json!({
                    "urls": url_list,
                    "urlCount": 1,
                    "successful": 0,
                    "error": error,
                    "responseId": response_id,
                })),
                ..Default::default()
            });
        }

        let full_length = result.content.chars().count();
        let slice = initial_content_slice(&result.content, max_inline);
        let truncated = slice.end_offset < full_length;
        let mut output = slice.text.clone();
        if truncated {
            output.push_str(&format!(
                "\n\n---\nShowing {} of {} chars, {} of {} bytes, and {} of {} lines. Use {TOOL_GET_SEARCH_CONTENT}({{ responseId: \"{response_id}\", urlIndex: 0, offset: {} }}) for the next slice.",
                slice.end_offset,
                full_length,
                slice.shown_bytes,
                byte_len(&result.content),
                slice.shown_lines,
                slice.total_lines,
                slice.end_offset
            ));
        }

        let title = if result.title.is_empty() {
            result.url.clone()
        } else {
            result.title.clone()
        };

        return Ok(ToolResult {
            text: output,
            details: Some(json!({
                "urls": url_list,
                "urlCount": 1,
                "successful": 1,
                "totalChars": full_length,
                "title": title,
                "responseId": response_id,
                "truncated": truncated,
                "mode": mode.as_str(),
                "mimeType": result.mime_type,
                "status": result.status,
            })),
            ..Default::default()
        });
    }

    let successful = fetched.iter().filter(|f| f.error.is_none()).count();
    let total_chars: usize = fetched.iter().map(|f| f.content.chars().count()).sum();
    let mut output = String::from("## Fetched URLs\n\n");

    for item in &fetched {
        match &item.error {
            Some(error) => output.push_str(&format!("- {}: Error - {error}\n", item.url)),
            None => output.push_str(&format!(
                "- {} ({} chars)\n",
                if item.title.is_empty() {
                    item.url.clone()
                } else {
                    item.title.clone()
                },
                item.content.chars().count()
            )),
        }
    }

    output.push_str(&format!(
        "\n---\nUse {TOOL_GET_SEARCH_CONTENT}({{ responseId: \"{response_id}\", urlIndex: 0 }}) to retrieve bounded content slices."
    ));

    Ok(ToolResult {
        text: output,
        details: Some(json!({
            "urls": url_list,
            "urlCount": url_list.len(),
            "successful": successful,
            "totalChars": total_chars,
            "responseId": response_id,
        })),
        ..Default::default()
    })
}

/// 合并单个 `url` 与数组 `urls` 参数，trim 后丢弃空串并去重，保持出现顺序。
fn url_list(args: &Value) -> Vec<String> {
    let mut urls: Vec<String> = Vec::new();
    if let Some(single) = args.get("url").and_then(|v| v.as_str())
        && !single.trim().is_empty()
    {
        urls.push(single.trim().to_string());
    }

    if let Some(items) = args.get("urls").and_then(|v| v.as_array()) {
        for item in items {
            if let Some(url) = item.as_str()
                && !url.trim().is_empty()
                && !urls.iter().any(|u| u == url.trim())
            {
                urls.push(url.trim().to_string());
            }
        }
    }
    urls
}

/// 本端口尚未实现本地代理探测，恒返回 `None`。
fn local_proxy_hint(_ctx: &ToolExecCtx) -> Option<String> {
    None
}

/// 由配置与调用参数合成抓取选项：显式 `proxy` 优先于配置值，
/// 并填充超时、User-Agent、私网放行与 SSRF 允许网段。
fn fetch_options(loaded: &LoadedConfig, mode: FetchMode, proxy: Option<&str>) -> FetchOptions {
    let cfg = &loaded.config;
    FetchOptions {
        mode,
        timeout: config::http_timeout(cfg),
        proxy: proxy
            .map(str::to_string)
            .or_else(|| cfg.proxy.clone())
            .filter(|p| !p.trim().is_empty()),
        user_agent: cfg.user_agent.clone().unwrap_or_else(default_user_agent),
        allow_private: cfg.allow_private_hosts.unwrap_or(false),
        allow_ranges: cfg.ssrf_allow_ranges.clone(),
    }
}

/// 以 `concurrency`（至少 1）为并发上限并行抓取全部 URL；
/// 返回与输入同序的结果，单项失败记录在各元素的 `error` 字段而不中止整批。
async fn fetch_many(
    urls: &[String],
    options: &FetchOptions,
    concurrency: usize,
) -> Vec<FetchedContent> {
    let owned: Vec<(String, FetchOptions)> = urls
        .iter()
        .map(|url| (url.clone(), options.clone()))
        .collect();
    futures_util::stream::iter(owned)
        .map(|(url, options)| async move { fetch::fetch_url(&url, &options).await })
        .buffered(concurrency.max(1))
        .collect()
        .await
}

/// 按字符数截断后的一段内容，附带结束偏移与行/字节统计。
struct ContentSlice {
    /// 按字符截断后的文本片段。
    text: String,
    /// 截断结束的字符偏移，同时作为下一段请求的 offset。
    end_offset: usize,
    /// 原文的总行数，用于展示「已显示/总数」统计。
    total_lines: usize,
    /// 已展示片段的字节数。
    shown_bytes: usize,
    /// 已展示片段的行数。
    shown_lines: usize,
}

/// 按字符数截断，尽量落在行边界。
fn initial_content_slice(content: &str, max_chars: usize) -> ContentSlice {
    let total_chars = content.chars().count();
    let mut end_offset = total_chars.min(max_chars);
    if end_offset < total_chars {
        // 在截断点之前找最后一个换行（按字符索引）
        let chars: Vec<char> = content.chars().collect();
        if let Some(position) = chars[..end_offset].iter().rposition(|c| *c == '\n')
            && position + 1 >= max_chars * 8 / 10
        {
            end_offset = position + 1;
        }
    }
    let text: String = content.chars().take(end_offset).collect();
    let total_lines = if content.is_empty() {
        0
    } else {
        content.split('\n').count()
    };
    let shown_lines = if text.is_empty() {
        0
    } else {
        text.split('\n').count()
    };
    let shown_bytes = byte_len(&text);
    ContentSlice {
        text,
        end_offset,
        total_lines,
        shown_bytes,
        shown_lines,
    }
}

/// 返回字符串的 UTF-8 字节数（区别于字符数）。
fn byte_len(text: &str) -> usize {
    text.len()
}

/// 执行 `get_search_content`：按 responseId 取回先前存储的搜索结果或抓取正文，
/// 支持按 query/queryIndex、url/urlIndex 定位，`findText` 检索片段或 offset/limit 分页。
/// responseId 缺失、无对应存储、定位越界或参数非法时返回 `Err`。
fn run_get_search_content(args: &Value) -> Result<ToolResult, ToolError> {
    let response_id = args
        .get("responseId")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ToolError("Error: 'responseId' is required.".to_string()))?
        .to_string();

    let find_text = find_text(args);
    let find_mode = match args.get("findMode") {
        None => FindMode::default(),
        Some(value) => FindMode::parse(value.as_str().unwrap_or_default()).ok_or_else(|| {
            ToolError(format!(
                "Invalid findMode: received {value}; findMode must be \"exact\", \"case-insensitive\", or \"fuzzy\"."
            ))
        })?,
    };
    if find_text.is_none() && args.get("findMode").is_some() {
        return Err(ToolError(format!(
            "findMode \"{find_mode}\" requires findText; provide findText or omit findMode."
        )));
    }

    let loaded = config::load();
    let max_inline = max_inline_content_chars(&loaded.config);

    let Some(data) = store::get(&response_id) else {
        return Err(ToolError(format!(
            "Error: No stored results for responseId \"{response_id}\". Use a responseId returned by {TOOL_WEB_SEARCH}/{TOOL_FETCH_CONTENT}."
        )));
    };

    match data {
        StoredData::Search { queries, .. } => {
            let query_data = select_query(&queries, args, &response_id)?;
            if let Some(error) = &query_data.error {
                return Err(ToolError(format!(
                    "Error retrieving query \"{}\" from responseId \"{response_id}\": {error}.",
                    query_data.query
                )));
            }
            let full = format_full_results(query_data);
            if let Some(queries) = find_text {
                return Ok(find_tool_result(&full, &queries, find_mode, |find| {
                    json!({
                        "query": query_data.query,
                        "resultCount": query_data.results.len(),
                        "findMode": find_mode.as_str(),
                        "matchCount": find.match_count,
                        "returnedMatches": find.returned_matches,
                    })
                }));
            }
            Ok(ToolResult {
                text: full,
                details: Some(json!({
                    "query": query_data.query,
                    "resultCount": query_data.results.len(),
                })),
                ..Default::default()
            })
        }
        StoredData::Fetch { urls, .. } => {
            let url_data = select_url(&urls, args, &response_id)?;
            if let Some(queries) = find_text {
                return Ok(find_tool_result(
                    &url_data.content,
                    &queries,
                    find_mode,
                    |find| {
                        json!({
                            "url": url_data.url,
                            "findMode": find_mode.as_str(),
                            "matchCount": find.match_count,
                            "returnedMatches": find.returned_matches,
                        })
                    },
                ));
            }
            let offset = match args.get("offset") {
                None => 0usize,
                Some(value) => value
                    .as_u64()
                    .map(|n| n as usize)
                    .ok_or_else(|| ToolError(format!("Invalid offset: received {value}.")))?,
            };
            let limit = match args.get("limit") {
                None => max_inline,
                Some(value) => {
                    let n = value
                        .as_u64()
                        .ok_or_else(|| ToolError(format!("Invalid limit: received {value}.")))?
                        as usize;
                    if n == 0 || n > max_inline {
                        return Err(ToolError(format!(
                            "Invalid limit: received {n}; limit must be an integer from 1 to {max_inline}."
                        )));
                    }
                    n
                }
            };
            let total = url_data.content.chars().count();
            if offset > total {
                return Err(ToolError(format!(
                    "Offset {offset} is out of range for responseId \"{response_id}\". Valid range is 0-{total}."
                )));
            }
            let end = (offset + limit).min(total);
            let slice: String = url_data
                .content
                .chars()
                .skip(offset)
                .take(end - offset)
                .collect();
            let has_more = end < total;
            Ok(ToolResult {
                text: slice.clone(),
                details: Some(json!({
                    "responseId": response_id,
                    "url": url_data.url,
                    "contentLength": total,
                    "offset": offset,
                    "limit": limit,
                    "returnedChars": slice.chars().count(),
                    "nextOffset": if has_more { json!(end) } else { json!(null) },
                    "truncated": has_more,
                })),
                ..Default::default()
            })
        }
    }
}

/// 在存储的查询列表里定位目标项：优先精确匹配 `query`，否则用 `queryIndex`；
/// 两者都未给出、找不到或越界时返回 `Err`（错误文案附可用查询清单）。
fn select_query<'a>(
    queries: &'a [QueryResultData],
    args: &Value,
    response_id: &str,
) -> Result<&'a QueryResultData, ToolError> {
    if let Some(query) = args.get("query").and_then(|v| v.as_str()) {
        return queries.iter().find(|q| q.query == query).ok_or_else(|| {
            let available = queries
                .iter()
                .map(|q| format!("\"{}\"", q.query))
                .collect::<Vec<_>>()
                .join(", ");
            ToolError(format!(
                "Query \"{query}\" was not found for responseId \"{response_id}\". Available queries: {}.",
                if available.is_empty() { "none".to_string() } else { available }
            ))
        });
    }
    if let Some(index) = args.get("queryIndex").and_then(|v| v.as_u64()) {
        return queries.get(index as usize).ok_or_else(|| {
            let available = queries
                .iter()
                .enumerate()
                .map(|(i, q)| format!("{i}: \"{}\"", q.query))
                .collect::<Vec<_>>()
                .join(", ");
            ToolError(format!(
                "Query index {index} is out of range for responseId \"{response_id}\". Valid indexes are 0-{}.",
                queries.len().saturating_sub(1)
            ) + &if available.is_empty() {
                String::new()
            } else {
                format!(" Available queries: {available}.")
            })
        });
    }
    let available = queries
        .iter()
        .enumerate()
        .map(|(i, q)| format!("{i}: \"{}\"", q.query))
        .collect::<Vec<_>>()
        .join(", ");
    Err(ToolError(format!(
        "Specify query or queryIndex for responseId \"{response_id}\". Available queries: {}.",
        if available.is_empty() {
            "none".to_string()
        } else {
            available
        }
    )))
}

/// 在存储的抓取结果里定位目标项：优先精确匹配 `url`，否则用 `urlIndex`（缺省 0）；
/// 找不到或越界时返回 `Err`。
fn select_url<'a>(
    urls: &'a [FetchedContent],
    args: &Value,
    response_id: &str,
) -> Result<&'a FetchedContent, ToolError> {
    if let Some(url) = args.get("url").and_then(|v| v.as_str()) {
        return urls.iter().find(|u| u.url == url).ok_or_else(|| {
            let available = urls.iter().map(|u| u.url.clone()).collect::<Vec<_>>().join("\n  ");
            ToolError(format!(
                "URL \"{url}\" was not found for responseId \"{response_id}\". Available URLs:\n  {available}"
            ))
        });
    }
    let index = args.get("urlIndex").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    urls.get(index).ok_or_else(|| {
        ToolError(format!(
            "URL index {index} is out of range for responseId \"{response_id}\". Valid indexes are 0-{}.",
            urls.len().saturating_sub(1)
        ))
    })
}

/// 解析 `findText` 参数（字符串或字符串数组），trim 并丢弃空项；
/// 无有效检索词时返回 `None`。
fn find_text(args: &Value) -> Option<Vec<String>> {
    let value = args.get("findText")?;
    let queries: Vec<String> = match value {
        Value::String(text) => vec![text.clone()],
        Value::Array(items) => items
            .iter()
            .filter_map(|v| v.as_str())
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    };
    let queries: Vec<String> = queries
        .into_iter()
        .map(|q| q.trim().to_string())
        .filter(|q| !q.is_empty())
        .collect();
    if queries.is_empty() {
        None
    } else {
        Some(queries)
    }
}

/// 用 [`find_content`] 在文本中检索关键词并渲染命中片段；
/// `details` 闭包据检索结果生成工具返回的 details。
fn find_tool_result(
    text: &str,
    queries: &[String],
    mode: FindMode,
    details: impl FnOnce(&FindResult) -> Value,
) -> ToolResult {
    let found = find_content(text, queries, mode, CONTEXT_CHARS, MAX_OUTPUT_CHARS);
    let details = details(&found);
    ToolResult {
        text: found.text.clone(),
        details: Some(details),
        ..Default::default()
    }
}

/// 把一条查询的完整结果（答案 + 每条结果的标题与链接）渲染为 markdown。
fn format_full_results(query: &QueryResultData) -> String {
    let mut output = format!("## Results for: \"{}\"\n\n", query.query);
    if !query.answer.is_empty() {
        output.push_str(&query.answer);
        output.push_str("\n\n---\n\n");
    }
    for result in &query.results {
        output.push_str(&format!("### {}\n{}\n\n", result.title, result.url));
    }
    output
}

/// 供扩展注册表在 `/reload` 时清理（保持与 config 缓存一致）。
pub fn invalidate_config() {
    config::invalidate();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_query_string_handles_json_arrays() {
        assert_eq!(
            expand_query_string("[\"a\", \"b\"]"),
            vec!["a".to_string(), "b".to_string()]
        );
        assert_eq!(expand_query_string("plain"), vec!["plain".to_string()]);
        assert_eq!(expand_query_string("[1,2]"), vec!["[1,2]".to_string()]);
    }

    #[test]
    fn query_list_prefers_queries_array() {
        let args = json!({ "query": "single", "queries": ["a", "b"] });
        assert_eq!(query_list(&args), vec!["a".to_string(), "b".to_string()]);
        let args = json!({ "query": "[\"x\",\"y\"]" });
        assert_eq!(query_list(&args), vec!["x".to_string(), "y".to_string()]);
    }

    #[test]
    fn initial_slice_breaks_on_line_boundary() {
        let content = "a".repeat(100) + "\n" + &"b".repeat(100);
        let slice = initial_content_slice(&content, 105);
        assert!(slice.text.ends_with('\n'));
        assert_eq!(slice.end_offset, 101);
        assert_eq!(slice.total_lines, 2);
    }

    #[test]
    fn format_summary_matches_upstream_shape() {
        let results = vec![SearchResult {
            title: "T".to_string(),
            url: "https://e.com".to_string(),
            snippet: "s".to_string(),
        }];
        let out = format_search_summary(&results, "answer");
        assert!(out.starts_with("answer"));
        assert!(out.contains("**Sources:**"));
        assert!(out.contains("1. T\n   https://e.com"));
        assert_eq!(format_search_summary(&[], ""), "No results found.");
    }

    #[test]
    fn fetch_content_rejects_unsupported_params() {
        let ctx = ToolExecCtx {
            execute_tool: crate::core::extensions::unavailable_tool_exec(),
            parent_tool_call_id: None,
            nested_calls: Default::default(),
            session_branch_entries: Default::default(),
            script_tools: Default::default(),
            cwd: ".".to_string(),
            make_sub_agent: Arc::new(|_| Err("unused".to_string())),
            parent_abort: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            agent_id: None,
            depth: 0,
            parent_model: None,
            script_call: false,
        };
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let err = runtime
            .block_on(run_fetch_content(
                &json!({ "url": "https://example.com", "prompt": "hi" }),
                &ctx,
            ))
            .unwrap_err();
        assert!(err.0.contains("prompt"), "{}", err.0);
        let err = runtime
            .block_on(run_fetch_content(&json!({ "urls": [] }), &ctx))
            .unwrap_err();
        assert!(err.0.contains("No URL"), "{}", err.0);
    }

    #[test]
    fn session_switch_clears_stored_content() {
        let _lock = store::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let ext = WebAccess;
        let id = store::store_search(vec![QueryResultData {
            query: "q".to_string(),
            answer: String::new(),
            results: vec![],
            error: None,
            provider: None,
        }]);
        assert!(store::get(&id).is_some());

        ext.on_session_switched(None, &[]);

        assert!(store::get(&id).is_none(), "切会话后旧 responseId 必须失效");
        let err = run_get_search_content(&json!({ "responseId": id })).unwrap_err();
        assert!(err.0.contains("No stored results"), "{}", err.0);
    }

    #[test]
    fn get_search_content_errors_are_actionable() {
        let err = run_get_search_content(&json!({})).unwrap_err();
        assert!(err.0.contains("responseId"));
        let err = run_get_search_content(&json!({ "responseId": "nope" })).unwrap_err();
        assert!(err.0.contains("No stored results"));
        let err = run_get_search_content(&json!({ "responseId": "nope", "findMode": "fuzzy" }))
            .unwrap_err();
        assert!(err.0.contains("requires findText"));
        let err = run_get_search_content(
            &json!({ "responseId": "nope", "findText": "x", "findMode": "bogus" }),
        )
        .unwrap_err();
        assert!(err.0.contains("Invalid findMode"), "{}", err.0);
    }
}
