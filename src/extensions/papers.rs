//! `papers` 扩展：文献检索与全文获取。
//!
//! 两个工具都声明为**延迟工具**（[`ToolExposure::Deferred`]）：不进模型可见列表，
//! 模型先经 `tool_search` 载入，才能在下一轮调用；扩展本身默认禁用，需在
//! `/extension` 面板开启。
//!
//! - `paper_search`：alphaXiv / OpenAlex / bioRxiv / PubMed 四个零鉴权源并发检索，
//!   按 id 合并去重后返回精简 JSON；
//! - `paper_get`：按 arXiv id / DOI / OpenAlex id / PMID 自路由取元数据 + Markdown 正文。
//!
//! 正文回退链：alphaXiv `abs.md`（全文）→ `overview.md`（综述）→ arXiv PDF →
//! OpenAlex 开放获取 PDF。PDF 只抽文本层（`pdf-extract`），不渲染版面、不识别图片，
//! 扫描件会得到「没有文本层」的说明。

mod sources;

use super::util::{opt_i64, opt_str, opt_string, opt_string_array, req_str, truncate_chars};
use crate::{
    core::{
        extensions::{
            Extension, ExtensionMode, ExtensionTool, ForkProjectInfo, ToolAnnotations, ToolExecCtx,
            ToolExposure,
        },
        tools::{ToolError, ToolResult},
    },
    extensions::{EXTENSION_FACTORIES, ExtensionFactory, PRIORITY_PAPERS},
};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use sources::{Id, Paper, Prioritize, SearchOptions, Source};
use std::{str::FromStr, sync::Arc};

/// 扩展名（也是 `/extension` 面板中的标识）。
const EXT: &str = "papers";
/// 检索工具名。
const TOOL_PAPER_SEARCH: &str = "paper_search";
/// 取文工具名。
const TOOL_PAPER_GET: &str = "paper_get";
/// 单个源的默认返回条数。
const DEFAULT_NUM_RESULTS: usize = 5;
/// 单个源的返回条数上限。
const MAX_NUM_RESULTS: usize = 20;
/// 检索结果里摘要的截断长度（字符）；正文靠 `paper_get` 取，这里只给够判断相关性的量。
const SUMMARY_CHARS: usize = 600;
/// 单次返回正文的字符上限，也是 `paper_get` 的 `limit` 上限。
const MAX_INLINE_CHARS: usize = 20_000;
/// 默认搜索源：alphaXiv（票数、只覆盖 arXiv）+ OpenAlex（跨库、引用数）。
const DEFAULT_SOURCES: [Source; 2] = [Source::Alphaxiv, Source::Openalex];

/// 注册进扩展工厂分布切片的 papers 扩展构造器。
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static PAPERS_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_PAPERS,
    make: || -> Arc<dyn Extension> { Arc::new(Papers) },
};

/// papers 扩展主体。
pub struct Papers;

impl Extension for Papers {
    /// 扩展名（固定 `papers`，用于工具分发与诊断）。
    fn name(&self) -> &str {
        EXT
    }

    /// 面板展示的一句话描述。
    fn description(&self) -> &str {
        "Literature search and full-text retrieval over alphaXiv, OpenAlex, bioRxiv and PubMed (paper_search / paper_get)."
    }

    /// 声明来源项目，供 `/extension` 面板展示。
    fn fork_project(&self) -> Option<ForkProjectInfo> {
        Some(ForkProjectInfo {
            plugin_name: "openresearch".to_string(),
            plugin_version: "0.2.23".to_string(),
            url: "https://github.com/alphaXiv/OpenResearch".to_string(),
        })
    }

    /// 仅在 Dev 与 Creator 两个并排模式下可用。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Dev, ExtensionMode::Creator]
    }

    /// 默认禁用：联网、吃带宽，且对「改代码」类任务是噪音。
    fn default_enabled(&self) -> bool {
        false
    }

    /// 两个工具都是延迟工具，模型得先经 `tool_search` 才能发现它们。
    fn prerequisite_tools(&self) -> Vec<String> {
        vec!["tool_search".to_string()]
    }

    /// 两个延迟工具：检索与取文。
    fn tools(&self) -> Vec<ExtensionTool> {
        vec![paper_search_tool(), paper_get_tool()]
    }

    /// 按工具名分发；未知工具名返回 `ToolError`。
    fn execute_tool_async(
        &self,
        name: String,
        args: Value,
        _ctx: ToolExecCtx,
    ) -> BoxFuture<'static, Result<ToolResult, ToolError>> {
        Box::pin(async move {
            match name.as_str() {
                TOOL_PAPER_SEARCH => run_paper_search(&args).await,
                TOOL_PAPER_GET => run_paper_get(&args).await,
                other => Err(ToolError(format!("extension {EXT}: unknown tool: {other}"))),
            }
        })
    }
}

/// 构造 `paper_search` 工具定义（描述、参数 JSON Schema 与 snippet）。
fn paper_search_tool() -> ExtensionTool {
    let mut tool = ExtensionTool::simple(
        TOOL_PAPER_SEARCH,
        "Search the scientific literature over zero-auth public sources: alphaXiv (community votes, arXiv only), OpenAlex (cross-publisher, citation counts, DOIs), bioRxiv (OpenAlex narrowed to bioRxiv) and PubMed (ranked by relevance). Sources are queried concurrently, merged and deduplicated by arXiv id / DOI / OpenAlex id / PMID. Returns a compact JSON array; pass an entry's `id` to paper_get for the abstract and full text.",
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query, passed to every source as-is."
                },
                "sources": {
                    "type": "array",
                    "items": { "type": "string", "enum": ["alphaxiv", "openalex", "biorxiv", "pubmed"] },
                    "description": "Sources to query; omit for alphaxiv + openalex. A source that fails is reported but does not fail the whole call."
                },
                "num_results": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_NUM_RESULTS,
                    "description": "Results per source (default: 5, max: 20)."
                },
                "published_after": {
                    "type": "string",
                    "description": "Earliest publication date, YYYY-MM-DD."
                },
                "published_before": {
                    "type": "string",
                    "description": "Latest publication date, YYYY-MM-DD."
                },
                "prioritize": {
                    "type": "string",
                    "enum": ["default", "recency", "popular", "historical"],
                    "description": "alphaXiv ranking only: recency (newest first), popular (most votes), historical (oldest first). Ignored by the other sources."
                }
            },
            "required": ["query"]
        }),
        "Use for scientific literature questions: returns papers with ids, citation counts and abstracts; follow up with paper_get for full text.",
    );
    tool.label = Some("Paper Search".to_string());
    tool.annotations = ToolAnnotations {
        read_only: true,
        ..ToolAnnotations::default()
    };
    tool.with_exposure(ToolExposure::Deferred)
}

/// 构造 `paper_get` 工具定义（描述、参数 JSON Schema 与 snippet）。
fn paper_get_tool() -> ExtensionTool {
    let mut tool = ExtensionTool::simple(
        TOOL_PAPER_GET,
        "Fetch one paper's metadata and body by id: an arXiv id (2401.12345), a DOI (10.1234/abc), an OpenAlex id (W123) or a PMID (PMID:123). The body comes from alphaXiv markdown when the paper is indexed there, otherwise from an arXiv or open-access PDF text layer; a scanned PDF yields an explicit \"no text layer\" note instead of empty content. Long bodies are truncated — pass `offset` to continue.",
        json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "arXiv id (2401.12345 / arXiv:2401.12345), DOI (10.1234/abc), OpenAlex id (W2787448102) or PMID (123 / PMID:123)."
                },
                "offset": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Character offset into the body (default 0)."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_INLINE_CHARS,
                    "description": "Maximum characters to return (default and maximum: 20000)."
                }
            },
            "required": ["id"]
        }),
        "Use after paper_search to read one paper: metadata plus body, with offset/limit paging for long texts.",
    );
    tool.label = Some("Paper Get".to_string());
    tool.annotations = ToolAnnotations {
        read_only: true,
        ..ToolAnnotations::default()
    };
    tool.with_exposure(ToolExposure::Deferred)
}

/// 执行 `paper_search`：并发查所有源，合并去重后输出精简 JSON 数组。
///
/// 单个源失败只记一行提示、其余源结果照常返回；**所有**源都失败才整体报错。
async fn run_paper_search(args: &Value) -> Result<ToolResult, ToolError> {
    let query = req_str(args, "query")?;
    let num_results = opt_i64(args, "num_results")
        .map(|n| n.clamp(1, MAX_NUM_RESULTS as i64) as usize)
        .unwrap_or(DEFAULT_NUM_RESULTS);
    let options = SearchOptions {
        query,
        num_results,
        after: opt_string(args, "published_after"),
        before: opt_string(args, "published_before"),
        prioritize: parse_prioritize(args)?,
    };
    let sources = parse_sources(args)?;
    let client = sources::client().map_err(ToolError)?;

    let outcomes = futures_util::future::join_all(sources.iter().map(|source| {
        let client = &client;
        let options = &options;
        async move { (*source, sources::search(*source, options, client).await) }
    }))
    .await;

    let mut papers = Vec::new();
    let mut errors = Vec::new();
    for (source, outcome) in outcomes {
        match outcome {
            Ok(found) => papers.extend(found),
            Err(err) => errors.push(format!("{source}: {err}")),
        }
    }
    if papers.is_empty() && !errors.is_empty() {
        return Err(ToolError(format!(
            "Error: every source failed — {}",
            errors.join("; ")
        )));
    }

    let merged = sources::merge(papers);
    if merged.is_empty() {
        return Ok(ToolResult::text(format!(
            "No results for \"{}\".",
            options.query
        )));
    }

    let items: Vec<Value> = merged.iter().map(search_item).collect();
    let mut text = serde_json::to_string_pretty(&Value::Array(items)).unwrap_or_default();
    if !errors.is_empty() {
        text.push_str(&format!("\n\nSome sources failed — {}", errors.join("; ")));
    }
    Ok(ToolResult::text(text))
}

/// 执行 `paper_get`：查元数据、走正文回退链，按 `offset` / `limit` 切片返回。
async fn run_paper_get(args: &Value) -> Result<ToolResult, ToolError> {
    let raw = req_str(args, "id")?;
    let id = Id::parse(&raw).ok_or_else(|| {
        ToolError(format!(
            "Error: cannot route id \"{raw}\"; pass an arXiv id (2401.12345), a DOI (10.1234/abc), an OpenAlex id (W123) or a PMID (PMID:123)."
        ))
    })?;
    let offset = opt_i64(args, "offset").unwrap_or(0).max(0) as usize;
    let limit = opt_i64(args, "limit")
        .map(|n| n.clamp(1, MAX_INLINE_CHARS as i64) as usize)
        .unwrap_or(MAX_INLINE_CHARS);

    let client = sources::client().map_err(ToolError)?;
    let paper = sources::lookup(&id, &client).await.map_err(ToolError)?;
    let (body, skipped) = fetch_body(&id, &paper, &client).await;

    let mut out = String::new();
    out.push_str(&format!("# {}\n\n", title_of(&paper, &id)));
    out.push_str(&format!("- id: {}\n", id.canonical()));
    if !paper.authors.is_empty() {
        out.push_str(&format!("- authors: {}\n", author_line(&paper.authors)));
    }
    if let Some(published) = &paper.published {
        out.push_str(&format!("- published: {published}\n"));
    }
    if let Some(doi) = &paper.doi {
        out.push_str(&format!("- doi: {doi}\n"));
    }
    if let Some(openalex) = &paper.openalex {
        out.push_str(&format!("- openalex: {openalex}\n"));
    }
    if let Some(pmid) = &paper.pmid {
        out.push_str(&format!("- pmid: {pmid}\n"));
    }
    if let Some(citations) = paper.citations {
        out.push_str(&format!("- citations: {citations}\n"));
    }
    if let Some(votes) = paper.votes {
        out.push_str(&format!("- votes: {votes}\n"));
    }
    if let Some(url) = &paper.url {
        out.push_str(&format!("- url: {url}\n"));
    }

    if !paper.summary.is_empty() {
        out.push_str(&format!("\n## Abstract\n\n{}\n", paper.summary));
    }

    match body {
        Some((text, origin)) => {
            let total = text.chars().count();
            out.push_str(&format!("\n## Full text ({origin}, {total} chars)\n\n"));
            out.push_str(&text.chars().skip(offset).take(limit).collect::<String>());
            let consumed = offset.saturating_add(limit);
            if consumed < total {
                out.push_str(&format!(
                    "\n\n[{} more characters; call paper_get with offset={} to continue]",
                    total - consumed,
                    consumed
                ));
            }
        }
        None => {
            out.push_str("\n## Full text\n\n");
            out.push_str(
                "No full text available: no alphaXiv markdown entry and no downloadable PDF.\n",
            );
            if !skipped.is_empty() {
                out.push_str(&format!("\nTried:\n{}\n", bullet_list(&skipped)));
            }
        }
    }
    Ok(ToolResult::text(out))
}

/// 检索结果里一条文献的精简 JSON。
fn search_item(paper: &Paper) -> Value {
    let sources: Vec<&'static str> = paper.sources.iter().map(|source| source.into()).collect();
    json!({
        "id": paper.canonical_id(),
        "title": paper.title,
        "authors": author_line(&paper.authors),
        "published": paper.published,
        "citations": paper.citations,
        "votes": paper.votes,
        "url": paper.url,
        "abstract": truncate_chars(&paper.summary, SUMMARY_CHARS),
        "sources": sources,
    })
}

/// 标题行：元数据没有标题时退回规范化 id。
fn title_of(paper: &Paper, id: &Id) -> String {
    if paper.title.is_empty() {
        id.canonical()
    } else {
        paper.title.clone()
    }
}

/// 作者行：最多列 8 位，其余折成 `et al.`。
fn author_line(authors: &[String]) -> String {
    const MAX_AUTHORS: usize = 8;
    if authors.len() <= MAX_AUTHORS {
        return authors.join("; ");
    }
    format!(
        "{}; et al. ({} authors)",
        authors[..MAX_AUTHORS].join("; "),
        authors.len()
    )
}

/// 把若干行折成 Markdown 无序列表。
fn bullet_list(lines: &[String]) -> String {
    lines
        .iter()
        .map(|line| format!("- {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 解析 `sources` 参数：省略时用 [`DEFAULT_SOURCES`]，未知取值报错（含合法取值）。
fn parse_sources(args: &Value) -> Result<Vec<Source>, ToolError> {
    let Some(names) = opt_string_array(args, "sources") else {
        return Ok(DEFAULT_SOURCES.to_vec());
    };
    if names.is_empty() {
        return Ok(DEFAULT_SOURCES.to_vec());
    }
    let mut sources: Vec<Source> = Vec::new();
    for name in names {
        let source = Source::from_str(name.trim()).map_err(|_| {
            ToolError(format!(
                "Error: unknown source \"{name}\"; use alphaxiv, openalex, biorxiv or pubmed."
            ))
        })?;
        if !sources.contains(&source) {
            sources.push(source);
        }
    }
    Ok(sources)
}

/// 解析 `prioritize` 参数（仅 alphaXiv 使用）。
fn parse_prioritize(args: &Value) -> Result<Option<Prioritize>, ToolError> {
    let Some(raw) = opt_str(args, "prioritize") else {
        return Ok(None);
    };
    Prioritize::from_str(raw.trim()).map(Some).map_err(|_| {
        ToolError(format!(
            "Error: unknown prioritize \"{raw}\"; use default, recency, popular or historical."
        ))
    })
}

/// 正文回退链的结果：命中的文本与来源说明（如 `alphaXiv abs.md`）。
type Body = (String, String);

/// 按回退链取正文：alphaXiv markdown（全文 → 综述）→ arXiv PDF → OpenAlex OA PDF。
///
/// 返回正文与「试过但没成」的说明列表；正文为 `None` 时说明列表用于告诉用户为什么。
async fn fetch_body(
    id: &Id,
    paper: &Paper,
    client: &reqwest::Client,
) -> (Option<Body>, Vec<String>) {
    let mut skipped = Vec::new();

    for (slug, kind) in markdown_candidates(id, paper) {
        let url = format!("https://www.alphaxiv.org/{kind}/{slug}.md");
        match sources::get_text(client, &url).await {
            Ok(Some(text)) if !text.trim().is_empty() => {
                return (Some((text, format!("alphaXiv {kind}.md"))), skipped);
            }
            Ok(_) => skipped.push(format!("{url}: empty or not found")),
            Err(err) => skipped.push(format!("{url}: {err}")),
        }
    }

    for (url, label) in pdf_candidates(id, paper) {
        match sources::get_bytes(client, &url).await {
            Ok(Some(bytes)) => match pdf_text(&bytes) {
                Ok(text) if !text.trim().is_empty() => {
                    return (Some((text, format!("{label}, PDF text layer"))), skipped);
                }
                Ok(_) => skipped.push(format!("{url}: PDF has no text layer")),
                Err(err) => skipped.push(format!("{url}: {err}")),
            },
            Ok(None) => skipped.push(format!("{url}: not found")),
            Err(err) => skipped.push(format!("{url}: {err}")),
        }
    }

    (None, skipped)
}

/// alphaXiv markdown 的候选 slug：arXiv id 优先，其次 DOI；每个 slug 先全文后综述。
fn markdown_candidates(id: &Id, paper: &Paper) -> Vec<(String, &'static str)> {
    let mut slugs: Vec<String> = Vec::new();
    if let Some(arxiv) = arxiv_of(id, paper) {
        slugs.push(arxiv);
    }
    if let Some(doi) = doi_of(id, paper)
        && !slugs.contains(&doi)
    {
        slugs.push(doi);
    }
    let mut candidates = Vec::new();
    for slug in slugs {
        candidates.push((slug.clone(), "abs"));
        candidates.push((slug, "overview"));
    }
    candidates
}

/// PDF 候选：arXiv 官方 PDF（`export.` 主机，arXiv 要求自动客户端用这个）优先，
/// 其次元数据里的开放获取 PDF。
fn pdf_candidates(id: &Id, paper: &Paper) -> Vec<(String, &'static str)> {
    let mut candidates = Vec::new();
    if let Some(arxiv) = arxiv_of(id, paper) {
        candidates.push((format!("https://export.arxiv.org/pdf/{arxiv}"), "arXiv PDF"));
    }
    if let Some(pdf) = &paper.pdf_url
        && candidates.iter().all(|(url, _)| url != pdf)
    {
        candidates.push((pdf.clone(), "open-access PDF"));
    }
    candidates
}

/// 取 arXiv id：先看查询用的 id，再看元数据。
fn arxiv_of(id: &Id, paper: &Paper) -> Option<String> {
    match id {
        Id::Arxiv(arxiv) => Some(arxiv.clone()),
        _ => paper.arxiv.clone(),
    }
}

/// 取 DOI：先看查询用的 id，再看元数据。
fn doi_of(id: &Id, paper: &Paper) -> Option<String> {
    match id {
        Id::Doi(doi) => Some(doi.clone()),
        _ => paper.doi.clone(),
    }
}

/// 从 PDF 字节抽文本层。
///
/// `pdf-extract` 对畸形文件会 panic，这里 `catch_unwind` 兜底（release 构建是
/// `panic = "abort"`，该兜底只在 debug / 测试构建下生效）。
fn pdf_text(bytes: &[u8]) -> Result<String, String> {
    let parsed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        pdf_extract::extract_text_from_mem(bytes)
    }));
    match parsed {
        Ok(Ok(text)) => Ok(text),
        Ok(Err(err)) => Err(format!("cannot extract PDF text: {err}")),
        Err(_) => Err("PDF parsing panicked; the file may be corrupt".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_definitions_are_deferred_and_read_only() {
        for tool in [paper_search_tool(), paper_get_tool()] {
            assert!(
                matches!(tool.exposure, ToolExposure::Deferred),
                "{}",
                tool.name
            );
            assert!(tool.annotations.read_only, "{}", tool.name);
        }
    }

    #[test]
    fn extension_is_off_by_default_and_needs_tool_search() {
        let extension = Papers;
        assert_eq!(extension.name(), EXT);
        assert!(!extension.default_enabled());
        assert_eq!(extension.prerequisite_tools(), vec!["tool_search"]);
        assert_eq!(extension.tools().len(), 2);
    }

    #[test]
    fn search_requires_a_query() {
        let err = parse_sources(&json!({ "sources": ["nope"] })).unwrap_err();
        assert!(err.0.contains("unknown source"), "{err:?}");
        assert_eq!(parse_sources(&json!({})).unwrap(), DEFAULT_SOURCES.to_vec());
        assert_eq!(parse_sources(&json!({ "sources": [] })).unwrap().len(), 2);
    }

    #[test]
    fn prioritize_accepts_the_four_server_values() {
        for (raw, want) in [
            ("default", Prioritize::Default),
            ("recency", Prioritize::Recency),
            ("popular", Prioritize::Popular),
            ("historical", Prioritize::Historical),
        ] {
            assert_eq!(
                parse_prioritize(&json!({ "prioritize": raw })).unwrap(),
                Some(want)
            );
        }
        assert!(parse_prioritize(&json!({ "prioritize": "votes" })).is_err());
        assert_eq!(parse_prioritize(&json!({})).unwrap(), None);
    }

    #[test]
    fn id_routing_handles_prefixes_and_bare_forms() {
        for (raw, want) in [
            ("2401.12345", Id::Arxiv("2401.12345".into())),
            ("arXiv:2401.12345v2", Id::Arxiv("2401.12345".into())),
            (
                "https://arxiv.org/abs/2401.12345",
                Id::Arxiv("2401.12345".into()),
            ),
            ("10.1101/251082", Id::Doi("10.1101/251082".into())),
            (
                "https://doi.org/10.1101/251082",
                Id::Doi("10.1101/251082".into()),
            ),
            ("W2787448102", Id::OpenAlex("W2787448102".into())),
            ("33535196", Id::Pubmed("33535196".into())),
            ("PMID:33535196", Id::Pubmed("33535196".into())),
        ] {
            assert_eq!(Id::parse(raw), Some(want), "{raw}");
        }
        assert_eq!(Id::parse("  "), None);
        assert_eq!(Id::parse("crispr paper"), None);
    }

    #[test]
    fn markdown_chain_tries_abs_before_overview() {
        let id = Id::Arxiv("2401.12345".into());
        let paper = Paper {
            doi: Some("10.1234/x".into()),
            ..Paper::default()
        };
        let candidates = markdown_candidates(&id, &paper);
        assert_eq!(
            candidates,
            vec![
                ("2401.12345".to_string(), "abs"),
                ("2401.12345".to_string(), "overview"),
                ("10.1234/x".to_string(), "abs"),
                ("10.1234/x".to_string(), "overview"),
            ]
        );
    }

    #[test]
    fn pdf_chain_prefers_arxiv_export_host() {
        let id = Id::Arxiv("2401.12345".into());
        let paper = Paper {
            pdf_url: Some("https://example.org/paper.pdf".into()),
            ..Paper::default()
        };
        let candidates = pdf_candidates(&id, &paper);
        assert_eq!(candidates[0].0, "https://export.arxiv.org/pdf/2401.12345");
        assert_eq!(candidates[1].0, "https://example.org/paper.pdf");
        assert_eq!(candidates.len(), 2);
    }

    #[test]
    #[ignore = "hits the public literature APIs"]
    fn live_search_and_get() {
        crate::test_support::run_with_timeout(std::time::Duration::from_secs(180), || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            runtime.block_on(async {
                let found = run_paper_search(&json!({
                    "query": "diffusion policy gradient",
                    "num_results": 3
                }))
                .await
                .expect("search");
                assert!(found.text.starts_with('['), "{}", found.text);

                let fetched = run_paper_get(&json!({ "id": "2609.06882", "limit": 2000 }))
                    .await
                    .expect("get");
                assert!(fetched.text.contains("## Full text"), "{}", fetched.text);

                let doi = run_paper_get(&json!({ "id": "10.1101/251082", "limit": 500 }))
                    .await
                    .expect("get by DOI");
                assert!(doi.text.contains("CRISPR"), "{}", doi.text);
            });
        });
    }

    #[test]
    fn author_line_folds_long_author_lists() {
        let authors: Vec<String> = (0..9).map(|i| format!("A{i}")).collect();
        assert!(author_line(&authors).ends_with("et al. (9 authors)"));
        assert_eq!(author_line(&authors[..2]), "A0; A1");
    }
}
