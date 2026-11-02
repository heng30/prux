# papers（文献检索与全文获取）

`papers` 扩展给 agent 补上「结构化文献」这一类信息：标题、作者、摘要、发表日期、
DOI / arXiv / PMID 自路由 id、引用数与社区票数，以及优先拿 Markdown 全文的能力。
普通网页检索拿得到网页，拿不到「哪些论文、哪篇原文」——这两件事不重叠。

**默认不启用**，且仅在 `Dev` / `Creator` 模式（及 `All`）下可用。它联网、吃带宽，
对「改代码」类任务是噪音。在 `/extension` 面板里开启，或写 `settings.json`：

```json
{ "enabledExtensions": ["papers"] }
```

两个工具都是**延迟工具**：启用后也不会直接出现在工具列表里，模型要先用 `tool_search`
检索到它们，才能在下一轮调用。所以 `tool_search` 也必须处于可用状态。

## 数据源

四个源都零鉴权，不需要 API key：

| 源 | 给什么 | 限制 |
|---|---|---|
| `alphaxiv` | 社区票数、摘要、命中片段 | 只索引 arXiv 预印本 |
| `openalex` | 被引次数、DOI、开放获取 PDF 地址、跨库检索 | 免费额度按 IP 共享，量大时可能被限流 |
| `biorxiv` | 只查 bioRxiv 的预印本 | 本质是 `openalex` 加来源过滤，不是独立端点 |
| `pubmed` | 生物医学文献，按相关度排序 | 两步请求（先排名拿 PMID，再取详情），比其它源慢 |

OpenAlex 有个可选的 polite pool 标识：把邮箱放进环境变量
`PRUX_OPENALEX_MAILTO`，请求就会带上 `mailto=` 参数；不设置也能用，只是享受不到
polite pool 的配额待遇。

## 工具

### `paper_search`

```json
{
  "query": "diffusion policy gradient",
  "sources": ["alphaxiv", "openalex", "biorxiv", "pubmed"],
  "num_results": 5,
  "published_after": "2024-01-01",
  "published_before": "2025-12-31",
  "prioritize": "popular"
}
```

只有 `query` 必填。`sources` 省略时查 `alphaxiv` + `openalex`（票数 + 引用数互补）；
`num_results` 是**每个源**的条数（默认 5，上限 20）。各源并发查询，按 arXiv id / DOI /
OpenAlex id / PMID 有交集即视为同一篇文献合并，票数与引用数取较大值。

`prioritize` 只对 `alphaxiv` 生效，取值 `default` / `recency` / `popular` / `historical`。
没有语义检索：这里不支持按向量相似度排序。

返回一个精简 JSON 数组，每个元素：

```json
{
  "id": "arxiv:2401.12345",
  "title": "…",
  "authors": "A; B; et al. (11 authors)",
  "published": "2024-01-12",
  "citations": 42,
  "votes": 11,
  "url": "https://arxiv.org/abs/2401.12345",
  "abstract": "…（截断到 600 字符）",
  "sources": ["alphaxiv", "openalex"]
}
```

失败语义：某个源出错只会在结果末尾追加一行 `Some sources failed — …`，其余源的结果
照常返回；**所有**源都失败才整体报错。没有任何命中时返回一句
`No results for "…".`（空数组不算错误）。

### `paper_get`

```json
{ "id": "10.1101/251082", "offset": 0, "limit": 20000 }
```

`id` 必填，按形状自路由，前缀可写可不写：

| 形状 | 例子 | 元数据来源 |
|---|---|---|
| arXiv id | `2401.12345` / `arXiv:2401.12345v2` / `https://arxiv.org/abs/2401.12345` | arXiv Atom 接口 |
| DOI | `10.1101/251082` / `https://doi.org/10.1101/251082` | OpenAlex |
| OpenAlex id | `W2787448102` | OpenAlex |
| PMID | `33535196` / `PMID:33535196` | PubMed |

输出是标题、id、作者、日期、DOI / PMID / OpenAlex id、引用数 / 票数、落地页 URL，
然后是摘要，最后是正文。

正文回退链，按顺序试，第一个成功的直接用：

1. alphaXiv 的 Markdown **全文**（`abs`）
2. alphaXiv 的 Markdown **综述**（`overview`，人工/模型生成的详细报告，不是原文）
3. arXiv PDF（走 `export.arxiv.org`，arXiv 要求自动客户端用这个主机）
4. OpenAlex 给的开放获取 PDF

PDF 只抽文本层，不渲染版面、不识别图片：扫描件会得到明确的「PDF 没有文本层」说明，
而不是一段空内容。正文超过 20000 字符时按字符截断，末尾提示剩余量与下一次的 `offset`；
继续读就带着新的 `offset` 再调一次（`limit` 上限也是 20000）。

全链都没拿到正文时，输出里会列出试过哪些地址、各自为什么不行。

不落存储、不做跨调用的分页状态：每次 `paper_get` 都是独立请求，`offset` 由调用方自己带。

## 代价与边界

- 每次 `paper_search` 都会实际打源站；`pubmed` 与 PDF 兜底会多花两三次请求。
- 单个响应体上限 32 MiB；超过即报错，不会把网络响应整段吃进内存。
- 只做检索与取文，不做文献管理、不订阅、不写本地库。
