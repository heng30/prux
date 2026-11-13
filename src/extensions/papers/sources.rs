//! `papers` 的数据源层：搜索（alphaXiv / OpenAlex / bioRxiv / PubMed）与元数据
//! （arXiv Atom / OpenAlex / PubMed）。
//!
//! 网络与解析分离：每个源一个 `parse_*` 纯函数，输入是响应体字符串、输出是
//! [`Paper`] 列表，解析层可以离线单测。四个源都零鉴权，不需要 API key。

use crate::{
    extensions::util::default_user_agent,
    utils::{
        display::flatten_line,
        http::{self, ClientOptions},
    },
};
use quick_xml::{
    Reader, XmlVersion,
    escape::resolve_predefined_entity,
    events::{BytesStart, Event},
};
use serde_json::Value;
use strum_macros::{Display, EnumString, IntoStaticStr};

/// OpenAlex polite pool 的 `mailto` 环境变量名；未设置时不发送该参数。
const OPENALEX_MAILTO_ENV: &str = "PRUX_OPENALEX_MAILTO";
/// bioRxiv 在 OpenAlex 里的 source id（用作 `primary_location.source.id` 过滤）。
const BIORXIV_SOURCE: &str = "S4306402567";
/// 单个响应体允许的最大字节数：一个坏响应不该把内存吃穿。
const MAX_BODY_BYTES: u64 = 32 * 1024 * 1024;

/// 搜索源。字符串形式即 `paper_search` 的 `sources` 取值。
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumString, Display, IntoStaticStr)]
#[strum(serialize_all = "lowercase")]
pub enum Source {
    /// alphaXiv：带社区票数与摘要，只索引 arXiv 预印本。
    Alphaxiv,
    /// OpenAlex：跨库的引用数、DOI、开放获取 PDF 地址。
    Openalex,
    /// bioRxiv：本质是 OpenAlex 加 `primary_location.source.id` 过滤，不是独立端点。
    Biorxiv,
    /// PubMed：`esearch` 排名 + `efetch` 取 XML。
    Pubmed,
}

/// alphaXiv 的排序策略（`prioritize` 查询参数）；服务端只接受这四个值。
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumString, Display, IntoStaticStr)]
#[strum(serialize_all = "lowercase")]
pub enum Prioritize {
    /// 服务端默认排序。
    Default,
    /// 按发表时间倒序。
    Recency,
    /// 按社区票数排序。
    Popular,
    /// 按发表时间正序（老文献优先）。
    Historical,
}

/// 一次搜索的参数（各源按自身能力取用）。
pub struct SearchOptions {
    /// 查询串，原样交给各源。
    pub query: String,
    /// 每个源最多返回多少条。
    pub num_results: usize,
    /// 发表时间下界（`YYYY-MM-DD`）；`None` = 不限。
    pub after: Option<String>,
    /// 发表时间上界（`YYYY-MM-DD`）；`None` = 不限。
    pub before: Option<String>,
    /// 仅 alphaXiv 支持的排序策略；`None` = 服务端默认。
    pub prioritize: Option<Prioritize>,
}

/// 一篇文献的统一条目。各源能填的字段不同，缺的留 `None` / 空。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Paper {
    /// 标题；源没给标题时为空串。
    pub title: String,
    /// 作者名列表，按源给的顺序。
    pub authors: Vec<String>,
    /// 摘要；源没给摘要时为空串。
    pub summary: String,
    /// 发表日期（`YYYY-MM-DD`，PubMed 只到月时是 `YYYY-MM`）。
    pub published: Option<String>,
    /// arXiv id（不含 `arXiv:` 前缀与版本号），如 `2401.12345`。
    pub arxiv: Option<String>,
    /// DOI（不含 `https://doi.org/` 前缀），如 `10.1101/251082`。
    pub doi: Option<String>,
    /// OpenAlex work id（形如 `W2787448102`）。
    pub openalex: Option<String>,
    /// PubMed id。
    pub pmid: Option<String>,
    /// 被引次数（仅 OpenAlex 提供）。
    pub citations: Option<u64>,
    /// 社区票数（仅 alphaXiv 提供）。
    pub votes: Option<i64>,
    /// 落地页 URL。
    pub url: Option<String>,
    /// 开放获取 PDF 地址（仅 OpenAlex 提供，且不保证可下载）。
    pub pdf_url: Option<String>,
    /// 命中该条目的源，按发现顺序去重。
    pub sources: Vec<Source>,
}

impl Paper {
    /// 用标题建一个空条目；其余字段由各源的解析函数填。
    fn new(title: &str) -> Self {
        Self {
            title: flatten_line(title),
            ..Self::default()
        }
    }

    /// 自路由 id：arXiv > DOI > OpenAlex > PMID，取第一个有的。
    ///
    /// 没有任何 id 时返回空串（调用方据此判断条目不可再查）。
    pub fn canonical_id(&self) -> String {
        if let Some(arxiv) = &self.arxiv {
            return format!("arxiv:{arxiv}");
        }
        if let Some(doi) = &self.doi {
            return format!("doi:{doi}");
        }
        if let Some(openalex) = &self.openalex {
            return format!("openalex:{openalex}");
        }
        if let Some(pmid) = &self.pmid {
            return format!("pmid:{pmid}");
        }
        String::new()
    }

    /// 全部可用于去重的键（arxiv / doi / openalex / pmid，缺的跳过）。
    fn keys(&self) -> Vec<String> {
        let mut keys = Vec::new();
        if let Some(arxiv) = &self.arxiv {
            keys.push(format!("arxiv:{arxiv}"));
        }
        if let Some(doi) = &self.doi {
            keys.push(format!("doi:{}", doi.to_lowercase()));
        }
        if let Some(openalex) = &self.openalex {
            keys.push(format!("openalex:{openalex}"));
        }
        if let Some(pmid) = &self.pmid {
            keys.push(format!("pmid:{pmid}"));
        }
        keys
    }

    /// 把另一条同文献的条目并进来：只补本条目缺的字段，票数与引用数取较大值。
    fn merge(&mut self, other: Paper) {
        if self.title.is_empty() {
            self.title = other.title;
        }
        if self.summary.is_empty() {
            self.summary = other.summary;
        }
        if self.authors.is_empty() {
            self.authors = other.authors;
        }
        if self.published.is_none() {
            self.published = other.published;
        }
        if self.arxiv.is_none() {
            self.arxiv = other.arxiv;
        }
        if self.doi.is_none() {
            self.doi = other.doi;
        }
        if self.openalex.is_none() {
            self.openalex = other.openalex;
        }
        if self.pmid.is_none() {
            self.pmid = other.pmid;
        }
        if self.url.is_none() {
            self.url = other.url;
        }
        if self.pdf_url.is_none() {
            self.pdf_url = other.pdf_url;
        }
        self.citations = self.citations.max(other.citations);
        self.votes = self.votes.max(other.votes);
        for source in other.sources {
            if !self.sources.contains(&source) {
                self.sources.push(source);
            }
        }
    }
}

/// 把多条源的搜索结果并成一个列表：按 [`Paper::keys`] 有交集即视为同一篇文献。
pub fn merge(papers: Vec<Paper>) -> Vec<Paper> {
    let mut merged: Vec<Paper> = Vec::new();
    for paper in papers {
        let keys = paper.keys();
        match merged
            .iter_mut()
            .find(|existing| existing.keys().iter().any(|key| keys.contains(key)))
        {
            Some(existing) => existing.merge(paper),
            None => merged.push(paper),
        }
    }
    merged
}

/// 待查文献的 id，决定元数据与正文走哪个源。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Id {
    /// arXiv id，如 `2401.12345`。
    Arxiv(String),
    /// DOI，如 `10.1101/251082`。
    Doi(String),
    /// OpenAlex work id，如 `W2787448102`。
    OpenAlex(String),
    /// PubMed id，如 `33535196`。
    Pubmed(String),
}

impl Id {
    /// 解析用户给的 id：接受裸 id 与常见前缀（`arXiv:` / `doi:` / `PMID:` /
    /// `https://arxiv.org/abs/` / `https://doi.org/`）。无法识别返回 `None`。
    pub fn parse(raw: &str) -> Option<Self> {
        let mut text = raw.trim();
        for prefix in [
            "https://arxiv.org/abs/",
            "http://arxiv.org/abs/",
            "https://www.arxiv.org/abs/",
            "https://doi.org/",
            "http://doi.org/",
            "https://dx.doi.org/",
            "https://openalex.org/",
            "arxiv:",
            "arXiv:",
            "doi:",
            "DOI:",
            "pmid:",
            "PMID:",
            "openalex:",
        ] {
            if let Some(rest) = text.strip_prefix(prefix) {
                text = rest.trim();
                break;
            }
        }
        if text.is_empty() {
            return None;
        }

        if text.starts_with("10.") && !text.contains(char::is_whitespace) {
            return Some(Self::Doi(text.to_string()));
        }
        if let Some(rest) = text.strip_prefix('W')
            && !rest.is_empty()
            && rest.chars().all(|c| c.is_ascii_digit())
        {
            return Some(Self::OpenAlex(text.to_string()));
        }
        if text.chars().all(|c| c.is_ascii_digit()) {
            return Some(Self::Pubmed(text.to_string()));
        }
        let core = text.split('v').next().unwrap_or(text);
        let (year, number) = core.split_once('.')?;
        let is_arxiv = year.len() == 4
            && year.chars().all(|c| c.is_ascii_digit())
            && number.len() >= 4
            && number.chars().all(|c| c.is_ascii_digit());
        is_arxiv.then(|| Self::Arxiv(core.to_string()))
    }

    /// 规范化 id（与 [`Paper::canonical_id`] 同一套前缀）。
    pub fn canonical(&self) -> String {
        match self {
            Self::Arxiv(id) => format!("arxiv:{id}"),
            Self::Doi(doi) => format!("doi:{doi}"),
            Self::OpenAlex(id) => format!("openalex:{id}"),
            Self::Pubmed(id) => format!("pmid:{id}"),
        }
    }
}

/// 建一个 papers 专用 HTTP 客户端：默认 30s 超时、跟随重定向，UA 与联网扩展同一套。
pub fn client() -> Result<reqwest::Client, String> {
    let user_agent = default_user_agent();
    http::build_client_with_options(&ClientOptions {
        user_agent: Some(user_agent.as_str()),
        pool_max_idle_per_host: 2,
        ..ClientOptions::default()
    })
}

/// 按源搜索；返回该源的结果，已截断到 `num_results`。
pub async fn search(
    source: Source,
    options: &SearchOptions,
    client: &reqwest::Client,
) -> Result<Vec<Paper>, String> {
    match source {
        Source::Alphaxiv => search_alphaxiv(options, client).await,
        Source::Openalex => search_openalex(options, client, None).await,
        Source::Biorxiv => search_openalex(options, client, Some(BIORXIV_SOURCE)).await,
        Source::Pubmed => search_pubmed(options, client).await,
    }
}

/// 按 id 取元数据。arXiv 走 Atom 接口，DOI / OpenAlex 走 OpenAlex，PMID 走 `efetch`。
pub async fn lookup(id: &Id, client: &reqwest::Client) -> Result<Paper, String> {
    match id {
        Id::Arxiv(arxiv) => {
            let url = http::with_query(
                "https://export.arxiv.org/api/query",
                &[("id_list", arxiv.as_str())],
            );
            let body = get_text(client, &url)
                .await?
                .ok_or_else(|| format!("arXiv has no entry for {arxiv}"))?;
            parse_arxiv_atom(&body)?
                .pop()
                .ok_or_else(|| format!("arXiv has no entry for {arxiv}"))
        }
        Id::Doi(doi) => {
            let url = format!("https://api.openalex.org/works/doi:{doi}");
            let value = http::get_json(client, &url, &[]).await?;
            openalex_paper(&value).ok_or_else(|| format!("OpenAlex has no work for DOI {doi}"))
        }
        Id::OpenAlex(work) => {
            let url = format!("https://api.openalex.org/works/{work}");
            let value = http::get_json(client, &url, &[]).await?;
            openalex_paper(&value).ok_or_else(|| format!("OpenAlex has no work {work}"))
        }
        Id::Pubmed(pmid) => {
            let url = http::with_query(
                "https://eutils.ncbi.nlm.nih.gov/entrez/eutils/efetch.fcgi",
                &[("db", "pubmed"), ("retmode", "xml"), ("id", pmid.as_str())],
            );
            let body = get_text(client, &url)
                .await?
                .ok_or_else(|| format!("PubMed has no record for {pmid}"))?;
            parse_pubmed(&body)?
                .pop()
                .ok_or_else(|| format!("PubMed has no record for {pmid}"))
        }
    }
}

/// GET 一个文本端点：404 返回 `Ok(None)`（正文回退链靠它判断「这个源没有」），
/// 其余非 2xx 与网络错误返回 `Err`。
pub async fn get_text(client: &reqwest::Client, url: &str) -> Result<Option<String>, String> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|err| format!("request failed: {err}"))?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !response.status().is_success() {
        return Err(http::error_summary(response).await);
    }
    response
        .text()
        .await
        .map(Some)
        .map_err(|err| format!("cannot read body: {err}"))
}

/// GET 一个二进制端点（PDF）：404 返回 `Ok(None)`，超过 [`MAX_BODY_BYTES`] 直接拒绝。
pub async fn get_bytes(client: &reqwest::Client, url: &str) -> Result<Option<Vec<u8>>, String> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|err| format!("request failed: {err}"))?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !response.status().is_success() {
        return Err(http::error_summary(response).await);
    }
    if response
        .content_length()
        .is_some_and(|len| len > MAX_BODY_BYTES)
    {
        return Err(format!("response is larger than {MAX_BODY_BYTES} bytes"));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|err| format!("cannot read body: {err}"))?;
    if bytes.len() as u64 > MAX_BODY_BYTES {
        return Err(format!("response is larger than {MAX_BODY_BYTES} bytes"));
    }
    Ok(Some(bytes.to_vec()))
}

/// alphaXiv 关键词检索（`discover/keyword`）：社区票数 + 摘要，只覆盖 arXiv。
///
/// 服务端会忽略过小的 `numResults`（实测给 2 回 25 条），因此结果在本地再截一次。
async fn search_alphaxiv(
    options: &SearchOptions,
    client: &reqwest::Client,
) -> Result<Vec<Paper>, String> {
    let limit = options.num_results.to_string();
    let prioritize = options.prioritize.map(|p| p.to_string());
    let mut params: Vec<(&str, &str)> = vec![("q", options.query.as_str()), ("numResults", &limit)];
    if let Some(prioritize) = &prioritize {
        params.push(("prioritize", prioritize.as_str()));
    }
    if let Some(after) = &options.after {
        params.push(("publishedAfter", after.as_str()));
    }
    if let Some(before) = &options.before {
        params.push(("publishedBefore", before.as_str()));
    }
    let url = http::with_query(
        "https://api.alphaxiv.org/search/v2/paper/discover/keyword",
        &params,
    );
    let value = http::get_json(client, &url, &[]).await?;
    let mut papers = parse_alphaxiv(&value)?;
    papers.truncate(options.num_results);
    Ok(papers)
}

/// OpenAlex works 检索；`source` 给出时按 `primary_location.source.id` 收窄（bioRxiv 就靠它）。
async fn search_openalex(
    options: &SearchOptions,
    client: &reqwest::Client,
    source: Option<&str>,
) -> Result<Vec<Paper>, String> {
    let limit = options.num_results.to_string();
    let mut filters: Vec<String> = Vec::new();
    if let Some(source) = source {
        filters.push(format!("primary_location.source.id:{source}"));
    }
    if let Some(after) = &options.after {
        filters.push(format!("from_publication_date:{after}"));
    }
    if let Some(before) = &options.before {
        filters.push(format!("to_publication_date:{before}"));
    }
    let filter = filters.join(",");
    let mailto = std::env::var(OPENALEX_MAILTO_ENV).ok();

    let mut params: Vec<(&str, &str)> =
        vec![("search", options.query.as_str()), ("per-page", &limit)];
    if !filter.is_empty() {
        params.push(("filter", filter.as_str()));
    }
    if let Some(mailto) = &mailto {
        params.push(("mailto", mailto.as_str()));
    }
    let url = http::with_query("https://api.openalex.org/works", &params);
    let value = http::get_json(client, &url, &[]).await?;
    let mut papers = parse_openalex(&value)?;
    papers.truncate(options.num_results);
    Ok(papers)
}

/// PubMed 两步检索：`esearch` 拿到按相关度排好的 PMID，再 `efetch` 取 XML。
async fn search_pubmed(
    options: &SearchOptions,
    client: &reqwest::Client,
) -> Result<Vec<Paper>, String> {
    let limit = options.num_results.to_string();
    let mut term = options.query.clone();
    if let Some(after) = &options.after {
        term.push_str(&format!(" AND {after}:3000[dp]"));
    }
    if let Some(before) = &options.before {
        term.push_str(&format!(" AND 1900:{before}[dp]"));
    }
    let url = http::with_query(
        "https://eutils.ncbi.nlm.nih.gov/entrez/eutils/esearch.fcgi",
        &[
            ("db", "pubmed"),
            ("retmode", "json"),
            ("sort", "relevance"),
            ("retmax", &limit),
            ("tool", "prux"),
            ("term", term.as_str()),
        ],
    );
    let value = http::get_json(client, &url, &[]).await?;
    let ids: Vec<String> = value["esearchresult"]["idlist"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let joined = ids.join(",");
    let url = http::with_query(
        "https://eutils.ncbi.nlm.nih.gov/entrez/eutils/efetch.fcgi",
        &[
            ("db", "pubmed"),
            ("retmode", "xml"),
            ("id", joined.as_str()),
        ],
    );
    let body = get_text(client, &url)
        .await?
        .ok_or_else(|| "PubMed returned no records for the matched ids".to_string())?;
    let mut papers = parse_pubmed(&body)?;
    papers.truncate(options.num_results);
    Ok(papers)
}

/// alphaXiv 响应 → 条目。响应是数组，元素含 `paperId` / `title` / `abstract` /
/// `publicationDate` / `votes`；`snippets`（命中片段）不取。
pub fn parse_alphaxiv(value: &Value) -> Result<Vec<Paper>, String> {
    let items = value
        .as_array()
        .ok_or_else(|| "alphaXiv returned no result array".to_string())?;
    Ok(items
        .iter()
        .filter_map(|item| {
            let paper_id = item.get("paperId")?.as_str()?;
            let mut paper = Paper::new(item.get("title")?.as_str()?);
            paper.summary = item
                .get("abstract")
                .and_then(|v| v.as_str())
                .map(flatten_line)
                .unwrap_or_default();
            paper.published = item
                .get("publicationDate")
                .and_then(|v| v.as_str())
                .map(|date| date.chars().take(10).collect());
            paper.votes = item.get("votes").and_then(|v| v.as_i64());
            paper.url = Some(format!("https://arxiv.org/abs/{paper_id}"));
            paper.arxiv = Some(paper_id.to_string());
            paper.sources.push(Source::Alphaxiv);
            Some(paper)
        })
        .collect())
}

/// OpenAlex `works` 响应 → 条目。摘要要从 `abstract_inverted_index` 还原。
pub fn parse_openalex(value: &Value) -> Result<Vec<Paper>, String> {
    let items = value
        .get("results")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "OpenAlex returned no results array".to_string())?;
    Ok(items.iter().filter_map(openalex_paper).collect())
}

/// 单条 OpenAlex work → 条目；缺 id 与标题时返回 `None`。
fn openalex_paper(item: &Value) -> Option<Paper> {
    let openalex = item
        .get("id")
        .and_then(|v| v.as_str())
        .map(|id| strip(id, "https://openalex.org/").to_string())?;
    let title = item
        .get("title")
        .and_then(|v| v.as_str())
        .or_else(|| item.get("display_name").and_then(|v| v.as_str()))
        .unwrap_or_default();
    let mut paper = Paper::new(title);
    paper.openalex = Some(openalex);
    paper.doi = item
        .get("doi")
        .and_then(|v| v.as_str())
        .map(|doi| strip(doi, "https://doi.org/").to_string());
    paper.pmid = item
        .get("ids")
        .and_then(|ids| ids.get("pmid"))
        .and_then(|v| v.as_str())
        .map(|pmid| pmid.rsplit('/').next().unwrap_or(pmid).to_string());
    // arXiv 预印本在 OpenAlex 里以 `10.48550/arXiv.<id>` 形式的 DOI 出现。
    paper.arxiv = paper.doi.as_deref().and_then(arxiv_from_doi);
    paper.summary = item
        .get("abstract_inverted_index")
        .map(abstract_from_inverted)
        .unwrap_or_default();
    paper.published = item
        .get("publication_date")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    paper.citations = item.get("cited_by_count").and_then(|v| v.as_u64());
    paper.authors = item
        .get("authorships")
        .and_then(|v| v.as_array())
        .map(|rows| {
            rows.iter()
                .filter_map(|row| row.get("author")?.get("display_name")?.as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let location = item
        .get("best_oa_location")
        .filter(|value| !value.is_null())
        .or_else(|| item.get("primary_location"));
    paper.pdf_url = location
        .and_then(|loc| loc.get("pdf_url"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    paper.url = location
        .and_then(|loc| loc.get("landing_page_url"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| {
            paper
                .doi
                .as_ref()
                .map(|doi| format!("https://doi.org/{doi}"))
        });
    paper.sources.push(Source::Openalex);
    Some(paper)
}

/// OpenAlex 的 `abstract_inverted_index`（词 → 位置数组）还原成摘要文本。
fn abstract_from_inverted(index: &Value) -> String {
    let Some(map) = index.as_object() else {
        return String::new();
    };
    let mut words: Vec<(u64, &str)> = Vec::new();
    for (word, positions) in map {
        for position in positions.as_array().into_iter().flatten() {
            if let Some(position) = position.as_u64() {
                words.push((position, word.as_str()));
            }
        }
    }
    words.sort_unstable_by_key(|(position, _)| *position);
    words
        .into_iter()
        .map(|(_, word)| word)
        .collect::<Vec<_>>()
        .join(" ")
}

/// PubMed `efetch` XML → 条目。
pub fn parse_pubmed(xml: &str) -> Result<Vec<Paper>, String> {
    let mut reader = Reader::from_str(xml);
    let mut path: Vec<String> = Vec::new();
    let mut text = String::new();
    let mut doi_tag = false;
    let mut current: Option<Paper> = None;
    let mut author: Option<(String, String)> = None;
    let mut date = [String::new(), String::new(), String::new()];
    let mut papers: Vec<Paper> = Vec::new();

    loop {
        let event = reader
            .read_event()
            .map_err(|err| format!("cannot parse PubMed XML: {err}"))?;
        match &event {
            Event::Eof => break,
            Event::Start(start) => {
                let name = local_name(start);
                if matches!(name.as_str(), "ELocationID" | "ArticleId") {
                    doi_tag = attr(start, "EIdType")
                        .or_else(|| attr(start, "IdType"))
                        .is_some_and(|kind| kind.eq_ignore_ascii_case("doi"));
                }
                if name == "PubmedArticle" {
                    current = Some(Paper::default());
                    date = Default::default();
                }
                path.push(name);
                text.clear();
            }
            Event::End(_) => {
                let name = path.pop().unwrap_or_default();
                let value = flatten_line(&text);
                let in_date = path.last().is_some_and(|parent| parent == "PubDate");
                if let Some(paper) = current.as_mut() {
                    match name.as_str() {
                        "PMID" if path.last().is_some_and(|p| p == "MedlineCitation") => {
                            paper.pmid = Some(value.clone());
                        }
                        "ArticleTitle" => paper.title = value.clone(),
                        "AbstractText" if !value.is_empty() => {
                            if !paper.summary.is_empty() {
                                paper.summary.push(' ');
                            }
                            paper.summary.push_str(&value);
                        }
                        "LastName" => author.get_or_insert_default().0 = value.clone(),
                        "ForeName" => author.get_or_insert_default().1 = value.clone(),
                        "CollectiveName" => paper.authors.push(value.clone()),
                        "Author" => {
                            if let Some((last, first)) = author.take() {
                                let name = match (last.is_empty(), first.is_empty()) {
                                    (false, false) => format!("{last}, {first}"),
                                    (false, true) => last,
                                    (true, false) => first,
                                    (true, true) => String::new(),
                                };
                                if !name.is_empty() {
                                    paper.authors.push(name);
                                }
                            }
                        }
                        "Year" if in_date => date[0] = value.clone(),
                        "Month" if in_date => date[1] = month_number(&value),
                        "Day" if in_date => date[2] = value.clone(),
                        "PubDate" => paper.published = format_date(&date),
                        "ELocationID" | "ArticleId" if doi_tag && !value.is_empty() => {
                            paper.doi = Some(strip(&value, "https://doi.org/").to_string());
                        }
                        _ => {}
                    }
                }
                doi_tag = false;
                if name == "PubmedArticle"
                    && let Some(mut paper) = current.take()
                {
                    paper.sources.push(Source::Pubmed);
                    if paper.url.is_none() {
                        paper.url = paper
                            .pmid
                            .as_ref()
                            .map(|pmid| format!("https://pubmed.ncbi.nlm.nih.gov/{pmid}/"));
                    }
                    papers.push(paper);
                }
                text.clear();
            }
            Event::Empty(_) => {
                text.clear();
                doi_tag = false;
            }
            Event::Text(_) | Event::CData(_) | Event::GeneralRef(_) => push_text(&event, &mut text),
            _ => {}
        }
    }
    Ok(papers)
}

/// arXiv Atom 响应 → 条目（`lookup` 只取第一条）。
pub fn parse_arxiv_atom(xml: &str) -> Result<Vec<Paper>, String> {
    let mut reader = Reader::from_str(xml);
    let mut path: Vec<String> = Vec::new();
    let mut text = String::new();
    let mut current: Option<Paper> = None;
    let mut papers: Vec<Paper> = Vec::new();

    loop {
        let event = reader
            .read_event()
            .map_err(|err| format!("cannot parse arXiv XML: {err}"))?;
        match &event {
            Event::Eof => break,
            Event::Start(start) => {
                let name = local_name(start);
                if name == "entry" {
                    current = Some(Paper::default());
                }
                path.push(name);
                text.clear();
            }
            Event::End(_) => {
                let name = path.pop().unwrap_or_default();
                let value = flatten_line(&text);
                if let Some(paper) = current.as_mut() {
                    match name.as_str() {
                        "id" => {
                            let arxiv = value
                                .rsplit('/')
                                .next()
                                .map(|id| id.split('v').next().unwrap_or(id).to_string());
                            paper.url = arxiv
                                .as_ref()
                                .map(|id| format!("https://arxiv.org/abs/{id}"));
                            paper.arxiv = arxiv;
                        }
                        "title" => paper.title = value.clone(),
                        "summary" => paper.summary = value.clone(),
                        "published" => {
                            paper.published = Some(value.chars().take(10).collect::<String>())
                        }
                        "name" if path.last().is_some_and(|p| p == "author") => {
                            paper.authors.push(value.clone())
                        }
                        "doi" => paper.doi = Some(strip(&value, "https://doi.org/").to_string()),
                        _ => {}
                    }
                }
                if name == "entry"
                    && let Some(mut paper) = current.take()
                {
                    paper.sources.push(Source::Alphaxiv);
                    papers.push(paper);
                }
                text.clear();
            }
            Event::Empty(_) => text.clear(),
            Event::Text(_) | Event::CData(_) | Event::GeneralRef(_) => push_text(&event, &mut text),
            _ => {}
        }
    }
    Ok(papers)
}

/// 去掉前缀；没有该前缀时原样返回。
fn strip<'a>(value: &'a str, prefix: &str) -> &'a str {
    value.strip_prefix(prefix).unwrap_or(value)
}

/// 从 `10.48550/arXiv.<id>` 形式的 DOI 里取 arXiv id（版本后缀一并去掉）。
fn arxiv_from_doi(doi: &str) -> Option<String> {
    let rest = doi
        .strip_prefix("10.48550/arXiv.")
        .or_else(|| doi.strip_prefix("10.48550/arxiv."))?;
    Some(rest.split('v').next().unwrap_or(rest).to_string())
}

/// 三字母英文月份 → 两位数字；已是数字或无法识别时原样返回。
fn month_number(month: &str) -> String {
    const MONTHS: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let lower = month.to_lowercase();
    match MONTHS.iter().position(|name| lower.starts_with(name)) {
        Some(index) => format!("{:02}", index + 1),
        None => month.to_string(),
    }
}

/// 把 `[年, 月, 日]` 拼成 `YYYY-MM-DD`；只有年时退成 `YYYY`，没有年返回 `None`。
fn format_date(date: &[String; 3]) -> Option<String> {
    let year = date[0].trim();
    if year.is_empty() {
        return None;
    }
    let mut out = year.to_string();
    for part in &date[1..] {
        let part = part.trim();
        if part.is_empty() {
            break;
        }
        out.push('-');
        out.push_str(part);
    }
    Some(out)
}

/// 开始标签的本地名（丢掉 `arxiv:` 之类前缀）。
fn local_name(start: &BytesStart<'_>) -> String {
    start.name().local_name().as_ref().to_string()
}

/// 按**本地名**取属性值；不存在或取值失败返回 `None`。
fn attr(start: &BytesStart<'_>, want: &str) -> Option<String> {
    start
        .attributes()
        .flatten()
        .find(|a| a.key.local_name().as_ref() == want)
        .and_then(|a| {
            a.normalized_value(XmlVersion::Implicit1_0)
                .ok()
                .map(|value| value.into_owned())
        })
}

/// 把 `Text` / `CData` / `GeneralRef` 三类事件的可见字符追加到 `out`。
///
/// `quick-xml` 把实体引用拆成独立的 `GeneralRef` 事件，三处都得处理，
/// 否则 `&amp;` 之类会整段消失。
fn push_text(event: &Event<'_>, out: &mut String) {
    match event {
        Event::Text(text) => out.push_str(text),
        Event::CData(data) => out.push_str(data),
        Event::GeneralRef(reference) => {
            if let Ok(Some(ch)) = reference.resolve_char_ref() {
                out.push(ch);
            } else if let Some(replacement) = resolve_predefined_entity(reference.as_ref()) {
                out.push_str(replacement);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::str::FromStr;

    /// alphaXiv `discover/keyword` 的响应形状（截自真实响应，去掉 `snippets`）。
    const ALPHAXIV: &str = r#"[{
        "paperId": "2609.06882",
        "title": "Noisy-Space Policy Gradient for Diffusion Policies\n  in Offline Reinforcement Learning",
        "abstract": "Diffusion policies offer a powerful parameterization for continuous control.",
        "publicationDate": "2026-09-07T00:08:13.000Z",
        "votes": 11
    }]"#;

    /// OpenAlex `works` 的响应形状（摘要用倒排索引给）。
    fn openalex_body() -> Value {
        json!({
            "results": [
                {
                    "id": "https://openalex.org/W2787448102",
                    "doi": "https://doi.org/10.1101/251082",
                    "title": "Inference of CRISPR Edits from Sanger Trace Data",
                    "publication_date": "2018-01-20",
                    "cited_by_count": 42,
                    "ids": { "openalex": "https://openalex.org/W2787448102", "doi": "https://doi.org/10.1101/251082" },
                    "authorships": [
                        { "author": { "display_name": "Hsiau, T." } },
                        { "author": { "display_name": "Conant, David" } }
                    ],
                    "abstract_inverted_index": { "ICE": [0], "scores": [1], "edits": [2] },
                    "best_oa_location": {
                        "landing_page_url": "https://doi.org/10.1101/251082",
                        "pdf_url": "https://www.biorxiv.org/content/251082.full.pdf"
                    }
                }
            ]
        })
    }

    /// PubMed `efetch` 的响应形状（含实体引用与集体作者）。
    const PUBMED: &str = r#"<?xml version="1.0" ?>
<PubmedArticleSet>
<PubmedArticle><MedlineCitation Status="Publisher" Owner="NLM"><PMID Version="1">42965688</PMID>
<Article PubModel="Print-Electronic"><Journal><JournalIssue CitedMedium="Internet">
<PubDate><Year>2026</Year><Month>Oct</Month><Day>31</Day></PubDate>
</JournalIssue></Journal>
<ArticleTitle>Dual-vector nanoengineered mRNA-CRISPR delivery.</ArticleTitle>
<ELocationID EIdType="doi" ValidYN="Y">10.1080/17435889.2026.2754253</ELocationID>
<Abstract><AbstractText>Choroidal melanoma is rare &amp; hard to treat.</AbstractText></Abstract>
<AuthorList>
<Author><LastName>Selim</LastName><ForeName>Mahmoud</ForeName></Author>
<Author><CollectiveName>ICE Consortium</CollectiveName></Author>
</AuthorList>
</Article></MedlineCitation></PubmedArticle>
</PubmedArticleSet>"#;

    /// arXiv Atom 的响应形状。
    const ARXIV: &str = r#"<?xml version='1.0' encoding='UTF-8'?>
<feed xmlns:arxiv="http://arxiv.org/schemas/atom" xmlns="http://www.w3.org/2005/Atom">
  <title>arXiv Query: search_query=&amp;id_list=2609.06882</title>
  <entry>
    <id>http://arxiv.org/abs/2609.06882v1</id>
    <title>Noisy-Space Policy Gradient</title>
    <published>2026-09-07T00:08:13Z</published>
    <author><name>Mahmoud Selim</name></author>
    <author><name>Cristina Cipriani</name></author>
    <arxiv:doi>10.48550/arXiv.2609.06882</arxiv:doi>
    <summary>Diffusion policies offer a powerful parameterization.</summary>
  </entry>
</feed>"#;

    #[test]
    fn alphaxiv_entries_become_papers() {
        let value: Value = serde_json::from_str(ALPHAXIV).unwrap();
        let papers = parse_alphaxiv(&value).unwrap();
        assert_eq!(papers.len(), 1);
        let paper = &papers[0];
        assert_eq!(paper.arxiv.as_deref(), Some("2609.06882"));
        assert_eq!(paper.canonical_id(), "arxiv:2609.06882");
        assert_eq!(paper.published.as_deref(), Some("2026-09-07"));
        assert_eq!(paper.votes, Some(11));
        assert_eq!(paper.sources, vec![Source::Alphaxiv]);
        assert!(!paper.title.contains('\n'), "{}", paper.title);
    }

    #[test]
    fn openalex_abstract_is_rebuilt_from_the_inverted_index() {
        let papers = parse_openalex(&openalex_body()).unwrap();
        let paper = &papers[0];
        assert_eq!(paper.summary, "ICE scores edits");
        assert_eq!(paper.citations, Some(42));
        assert_eq!(paper.doi.as_deref(), Some("10.1101/251082"));
        assert_eq!(paper.authors, vec!["Hsiau, T.", "Conant, David"]);
        assert_eq!(
            paper.pdf_url.as_deref(),
            Some("https://www.biorxiv.org/content/251082.full.pdf")
        );
        // arXiv DOI 形式要能反解出 arXiv id。
        assert_eq!(
            arxiv_from_doi("10.48550/arXiv.2401.12345v3").as_deref(),
            Some("2401.12345")
        );
        assert_eq!(arxiv_from_doi("10.1101/251082"), None);
    }

    #[test]
    fn openalex_without_id_is_skipped() {
        let value = json!({ "results": [ { "title": "no id" } ] });
        assert!(parse_openalex(&value).unwrap().is_empty());
    }

    #[test]
    fn pubmed_xml_yields_metadata() {
        let papers = parse_pubmed(PUBMED).unwrap();
        assert_eq!(papers.len(), 1);
        let paper = &papers[0];
        assert_eq!(paper.pmid.as_deref(), Some("42965688"));
        assert_eq!(
            paper.title,
            "Dual-vector nanoengineered mRNA-CRISPR delivery."
        );
        assert_eq!(paper.summary, "Choroidal melanoma is rare & hard to treat.");
        assert_eq!(paper.published.as_deref(), Some("2026-10-31"));
        assert_eq!(paper.doi.as_deref(), Some("10.1080/17435889.2026.2754253"));
        assert_eq!(paper.authors, vec!["Selim, Mahmoud", "ICE Consortium"]);
        assert_eq!(
            paper.url.as_deref(),
            Some("https://pubmed.ncbi.nlm.nih.gov/42965688/")
        );
    }

    #[test]
    fn arxiv_atom_yields_metadata() {
        let papers = parse_arxiv_atom(ARXIV).unwrap();
        assert_eq!(papers.len(), 1);
        let paper = &papers[0];
        assert_eq!(paper.title, "Noisy-Space Policy Gradient");
        assert_eq!(paper.arxiv.as_deref(), Some("2609.06882"));
        assert_eq!(paper.published.as_deref(), Some("2026-09-07"));
        assert_eq!(paper.authors, vec!["Mahmoud Selim", "Cristina Cipriani"]);
        assert_eq!(paper.doi.as_deref(), Some("10.48550/arXiv.2609.06882"));
    }

    #[test]
    fn merge_folds_the_same_work_across_sources() {
        let alphaxiv = json!([{
            "paperId": "2401.12345",
            "title": "Same paper",
            "abstract": "",
            "publicationDate": "2024-01-01",
            "votes": 7
        }]);
        let mut openalex = openalex_body();
        openalex["results"][0]["doi"] = json!("https://doi.org/10.48550/arXiv.2401.12345");
        openalex["results"][0]["abstract_inverted_index"] = json!({ "abstract": [0], "text": [1] });

        let mut papers = parse_alphaxiv(&alphaxiv).unwrap();
        papers.extend(parse_openalex(&openalex).unwrap());
        let merged = merge(papers);

        assert_eq!(merged.len(), 1);
        let paper = &merged[0];
        assert_eq!(paper.votes, Some(7));
        assert_eq!(paper.citations, Some(42));
        assert_eq!(paper.summary, "abstract text");
        assert_eq!(paper.authors.len(), 2);
        assert_eq!(paper.sources, vec![Source::Alphaxiv, Source::Openalex]);
    }

    #[test]
    fn canonical_id_prefers_arxiv_over_doi() {
        let paper = Paper {
            arxiv: Some("2401.12345".into()),
            doi: Some("10.1234/x".into()),
            ..Paper::default()
        };
        assert_eq!(paper.canonical_id(), "arxiv:2401.12345");
        assert_eq!(Id::Doi("10.1234/x".into()).canonical(), "doi:10.1234/x");
    }

    #[test]
    fn month_names_and_partial_dates_are_formatted() {
        assert_eq!(month_number("Oct"), "10");
        assert_eq!(month_number("12"), "12");
        assert_eq!(month_number("weird"), "weird");
        assert_eq!(
            format_date(&["2026".into(), "10".into(), String::new()]),
            Some("2026-10".to_string())
        );
        assert_eq!(
            format_date(&[String::new(), "10".into(), "31".into()]),
            None
        );
    }

    #[test]
    fn source_names_round_trip_through_strum() {
        for (name, source) in [
            ("alphaxiv", Source::Alphaxiv),
            ("openalex", Source::Openalex),
            ("biorxiv", Source::Biorxiv),
            ("pubmed", Source::Pubmed),
        ] {
            assert_eq!(Source::from_str(name).unwrap(), source);
            assert_eq!(source.to_string(), name);
        }
    }
}
