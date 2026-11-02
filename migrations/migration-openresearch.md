# Prux 迁移指南：OpenResearch → prux 扩展（候选清单 + `papers` 设计稿）

> **来源**：`OpenResearch`（`https://github.com/alphaXiv/OpenResearch`，Rust 写的
> 「本地优先的科研 agent harness / workspace」；本仓的本地只读检出是
> `OpenResearch -> /home/blue/wayshot-fork/OpenResearch`，检出点 `a92fd471`（2026-10-30），
> `Cargo.toml` 的 `version = "0.2.23"`，`LICENSE` 是 MIT）。
>
> **目标**：`src/extensions/` 下的内置扩展（不与上游插件移植混在一起；本仓已有
> `migration-subagents.md` / `migration-tasks.md` / `migration-kiss-v0.0.35.md` /
> `migration-jcode-v0.94.0.md` / `migration-jan.md`）。
> **性质**：先冻结「值得移植 / 不值得移植」的判断，再逐条落地；每落地一项，更新路线图状态
> 并在该节末尾补「落地记录」。
>
> **判断口径**（沿用 `migration-jcode-v0.94.0.md` 的尺子，并补第三条）：
> 一个东西算「可作为扩展」，当且仅当它是**一项独立功能、自带用户可见价值**。
> 满足以下任一条的一律明确不做：
> 1. 它是**机制 / 策略**而不是功能——该并进已有扩展或 core，不新起一个；
> 2. 它需要**服务端 / 守护进程 / 新运行模式 / 新 provider 适配 / 账号体系**——扩展层够不到；
> 3. 它只在**桌面 GUI 或浏览器**里有意义——prux 是终端，宿主不匹配即淘汰。
>
> **与既有迁移文档的关系**：OpenResearch 不是 pi 的重写，它是**另一种形态**的 agent 产品
> （科研工作台，围绕实验与文献）。所以本文档记的不是「同一契约的两套实现」，
> 而是「它能长出、prux 还没有的一类能力」。凡是与 pi 契约相关的差异，以 `migration-v*.md` 为准。

---

## 〇、摘要

### 结论

OpenResearch 的价值集中在两处，**只有一处能作为扩展落地**：

- **文献检索与全文获取**（§2.1）：四个**零鉴权**公开端点（alphaXiv / OpenAlex / PubMed /
  bioRxiv）+ 一条 `.md` 优先、PDF 兜底的正文回退链。prux 完全没有这类结构化文献能力
  （`web_access` 是通用搜索 + 网页取文，拿不到作者/引用数/摘要/DOI 自路由）。
  → 设计成 `papers` 扩展，默认不启用、工具延迟暴露。
- **长任务外包**（§3.1）：`runs` 表 + `RunStatus` 六态 + `orx supervise` 轮询 +
  `ComputeBackend` trait + 源码 tar 的 SHA-256 内容寻址。工程上很干净，但它需要
  **SQLite store + 独立进程 + 11 个后端**才能成立，与 prux 的 `tasks` / `ssh_remote`
  两个扩展在语义上已经重叠。→ 不做，只在 §3.1 记下三条值得偷的细节。

其余部分（React dashboard、axum 本地服务、桌面壳、账号与 compute 云、遥测、自更新、
五家 harness 适配、Overleaf cookie 解密）对终端 agent 无意义。

**候选清单至此收口：落地 1 项（`papers`，已实现），明确不做 7 项，无待做项。**

### 路线图

| # | 项 | 类型 | 状态 |
|---|---|---|---|
| 2.1 | `papers` 扩展（文献检索 + 全文获取） | 新扩展 | **已落地**（`src/extensions/papers.rs`，实现与设计稿的偏差见 §2.1 落地记录） |
| 3.1 | `runs` / `supervise` / `ComputeBackend` | — | 不做 |
| 3.2 | `experiment` = git 分支树 | — | 不做（与 `rewind` / 子代理 worktree 重叠） |
| 3.3 | skill 随二进制分发 + 装到各家 skills 目录 | — | 不做（`skills` 扩展已有） |
| 3.4 | `workspace_state` / `browser` / `editors` | — | 不做（宿主能力 / 一行 shell） |
| 3.5 | `orx up` 本地 dashboard + axum + SSE | — | 不做（GUI） |
| 3.6 | 账号、compute 云、telemetry、自更新 | — | 不做（服务端） |
| 3.7 | 五家 harness 适配 | — | 不做（prux 就是 harness） |

---

## 一、它的结构，与 prux 的重叠面

| OpenResearch | 做什么 | prux 的对应物 |
|---|---|---|
| `src/local/`（`orx up`） | 起本地 dashboard，把 Claude Code / Codex / OpenCode / Cursor / Antigravity 当执行器，管 chat / 实验 / skills / overleaf / latex | prux 自己就是执行器 |
| `agent-skills/<name>/SKILL.md`（15 个） | 用 `include_str!` 嵌进二进制，`orx skill <name>` 打印，`orx install-skills` 写进各 harness 的 skills 目录 | `skills` 扩展 |
| `SYSTEM_PROMPT.md` | `orx up` 注入的 playbook，带 `{name}/{id}/{compute_bullet}/{artifacts}` 模板变量 | `core::system_prompt` |
| `src/client.rs` | 文献检索四源 + 论文正文 + 账号/compute API | **无**（§2.1） |
| `src/jobs/` + `compute.rs` + `commands/supervise.rs` | 11 个执行后端，`runs` 表、轮询、取消、日志 | `tasks` + `ssh_remote`（语义重叠，粒度不同） |
| `src/store.rs` | SQLite（rusqlite bundled）：projects / experiments / runs / chat_* / usage | 会话是 JSONL 文件，无 SQLite |
| `src/plane.rs` | 本地 project→experiment→run 的展示层 | 无 |
| `src/browser.rs` / `editors.rs` / `folder_picker.rs` | 用默认浏览器开 URL、用默认应用开文件、选目录 | `open` 扩展 / `@` 补全 |
| `src/remote.rs` | 检测 `SSH_CONNECTION`，提示 `ssh -L` 访问 dashboard | `ssh_remote`（正相反：本地驱动远端） |
| `workspace_state.rs` | **GUI 布局状态**（tab / pane / 宽度 / 滚动 / 缩放），与科研产物无关 | 无 |
| `updates/` / `telemetry/` / `login` / `orgs` / `feedback` | cargo-dist 自更新、匿名遥测、alphaXiv 账号 | `update_check` 扩展 / `auth` 子命令 |
| `ui/` + `macos/` + `windows/` + `linux/` | React SPA（44 MB）、Tauri 桌面打包 | `modes/gui`（feature gated） |

---

## 二、值得移植

### 2.1 `papers` 扩展：文献检索 + 全文获取

**为什么它是独立功能**：它会新增「结构化文献」这一整类信息——标题、作者、摘要、
发表日期、DOI/arXiv/PMID 的自路由 id、引用数、社区票数，以及**优先拿 Markdown 全文**的
能力。这与 `web_access` 的通用搜索是两件事：后者拿得到网页，拿不到「哪些论文、哪篇原文」。

#### 数据源（全部零鉴权，直接照抄语义）

| 源 | 端点 | 备注 |
|---|---|---|
| alphaXiv | `GET https://api.alphaxiv.org/search/v2/paper/discover/keyword?q=…` | 支持 `prioritize` 与 `publishedAfter` / `publishedBefore`；返回社区票数与命中片段 |
| OpenAlex | `GET https://api.openalex.org/works?…` | 带 `mailto=` 走 polite pool；引用数来自这里 |
| bioRxiv | 同上 OpenAlex + `source:S4306402567` 过滤；详情用 `https://api.biorxiv.org/details/biorxiv/<doi>/na/json` | 不是独立搜索 |
| PubMed | `esearch.fcgi` 排名 → `efetch.fcgi` 取 XML | 带 `tool=` / `email=`；XML 用已有的 `quick-xml` 解析，不引新依赖 |

**注意**：alphaXiv 还有 `/discover/embedding` 变体（按语义排序）。prux 没有 embedding 栈
（与 `migration-jan.md` §3.1 同一条约束），**只用 keyword 变体**，不要假装能做语义检索。

#### 正文获取的回退链（这是最值得照抄的一段）

`https://www.alphaxiv.org/overview/<id>.md`（综述）→ 404 时退
`https://www.alphaxiv.org/abs/<id>.md`（全文）→ 仍不可用则拿 PDF：
`https://export.arxiv.org/pdf/<id>`（arXiv 明确要求自动客户端用 `export.` 主机）。
PDF 落临时文件后交给已有的 `markit` 扩展的 `read_doc` 转 Markdown，**不要自己写 PDF 解析**。

#### 建议形态

- **扩展名** `papers`，默认**不启用**（`Dev` / `Creator` 模式）；理由：它联网、会花用户带宽，
  且对「改代码」类任务是噪音。
- **两个工具**，都 `ToolExposure::Deferred`（模型先用 `tool_search` 才能发现，与 `ssh_remote`
  的七个工具同一处理）：
  - `paper_search{ query, sources?, num_results?, published_after?, published_before?, prioritize? }`
    → 精简 JSON 数组；`sources` 省略 = alphaXiv + OpenAlex 并发合并并按 id 去重；
  - `paper_get{ id }` → 自路由：arXiv id（`2401.12345` / `arXiv:…`）/ DOI（`10.…`）/
    OpenAlex `W…` / PMID → 拿元数据 + Markdown 正文（截断到固定上限）。
- **不做存储**：不引 responseId 分页机制（那是 `web_access` 为网页正文做的），
  单次返回即可；正文超长按字符截断，尾部提示用 `paper_get` 的 `offset` 参数继续取
  （与 `get_search_content` 的 offset/limit 语义保持一致）。
- **失败语义与 pi 文案一致**：无命中 → 空数组 + 一句说明；网络失败 → 错误里点名是哪个源失败，
  且**只有一个源失败时不整体失败**（其余源的结果照常返回）。

#### 验证

1. 解析层单测：OpenAlex `works` JSON → 统一条目、PubMed `efetch` XML → 统一条目、
   alphaXiv 命中 → 统一条目（各一份 fixtures，写进 `tests/` 或模块内 `#[cfg(test)]`）；
2. 回退链单测：mock 一个本地 HTTP 服务，`overview.md` 返回 404 时断言走 `abs.md`；
3. 网络集成测试用 `run_with_timeout` 包住，并在无网环境下跳过而不是失败。

#### 落地记录

已落地（`src/extensions/papers.rs` + `src/extensions/papers/sources.rs`，文档
`assets/docs/extensions/papers.md`）。两个延迟工具、四源并发合并、id 自路由、正文回退链
都按本节的形态实现，解析层有离线单测（alphaXiv / OpenAlex / PubMed / arXiv Atom 各一份
fixture），另有一条 `#[ignore]` 的真实网络冒烟测试
（`cargo test --lib extensions::papers -- --ignored`）。实现时按实测端点修正了设计稿的
五处，理由如下：

1. **`prioritize` 的合法取值不是自由字符串**：服务端只接受
   `default` / `recency` / `popular` / `historical`，传 `votes` 直接 400。已收成枚举
   （`strum`），非法值在参数校验阶段报错。
2. **回退链把 `abs.md` 放到 `overview.md` 前面**：`overview/<id>.md` 不是原文，是
   「Detailed Report」形态的生成式综述。先给综述会让模型把二手总结当论文正文。
   现在先拿 `abs.md`（真实全文），404 时才退综述，再退 PDF。
3. **PDF 不落临时文件、不经 `markit` 的 `read_doc`**：两者同 crate，直接调
   `pdf_extract::extract_text_from_mem`（`catch_unwind` 兜底，与 `markit` 同一口径），
   省掉一次落盘和一次跨扩展工具调用。
4. **不用 bioRxiv 的 `details` 端点**：OpenAlex 已经给了标题 / 作者 / 日期 / 摘要，
   `api.biorxiv.org/details` 只会把一次搜索变成 N+1 次请求。bioRxiv 保持为「OpenAlex
   加来源过滤」。
5. **arXiv 元数据走 arXiv Atom 接口**：alphaXiv 没有 by-id 端点
   （`paper/v1|v2/<id>` 全 404），`paper_get` 按 arXiv id 查元数据只能问 arXiv 自己。

另外两处设计稿没写、实现时定下的口径：OpenAlex 的 `mailto` 取环境变量
`PRUX_OPENALEX_MAILTO`（不硬编码任何邮箱）；回退链的顺序单测只断言候选序列，没有起本地
mock HTTP 服务——链里的主机是硬编码常量，为一条顺序逻辑注入 host 不值得，真实链路由上面
那条 ignored 冒烟测试覆盖。

---

## 三、明确不做

### 3.1 `runs` / `orx supervise` / `ComputeBackend`

这是 OpenResearch 最完整的一块：`runs` 表（`src/store.rs`）存 `status`、
`backend_json`（`BackendDescriptor`）、`exit_code`、`ended_at`，日志落
`data_dir/run-logs/<id>.log`；`RunStatus` 六态
`starting / running / done / failed / cancelled / lost`，终止态不可变；
`orx supervise <runId>` 是**独立进程**，5 秒轮询、tail 日志、读本地 cancel 意图；
`ComputeBackend` trait 统一 `preflight / stage_source / submit / status / logs / cancel / cleanup`，
源码打 tar 后按 SHA-256 内容寻址（不 clone git 历史）。

**不做的理由**：它成立需要三样 prux 都没有的东西——SQLite store、常驻 supervise 进程、
以及 11 个后端里的云账号（hf / modal / k8s / slurm / ray / tinker / rental / openresearch）。
而 prux 已有的能力在语义上覆盖了它：`tasks` 扩展管长任务与状态，`ssh_remote` 提供
`remote_exec` + `remote_sync`。硬塞进来会得到两套任务状态机。

**三条值得偷、但归并进 `tasks` 的细节**（本档判为**不做**，仅记录，翻案条件写在后面）：
1. **容忍抖动**：监控连续失败超过 60 秒才把 `monitoring_error` 写进状态，避免一次网络抖动
   就把任务判死；
2. **终止态不可变**：`done / failed / cancelled / lost` 一律不再被后续状态更新覆盖，
   提交时的 upsert 也遵守这条——这是长任务状态机最容易写错的地方；
3. **内容寻址快照**：传源码用 tar + SHA-256 而不是 git 历史，远端不需要是 git 仓库。

**翻案条件**：`tasks` 扩展开始支持「任务跨进程存活」（prux 退出后仍能查状态）。

### 3.2 `project → experiment(git 分支) → run` 的树

每个 experiment 是一条 git 分支 / 一个 worktree，`parent_experiment_id` 形成树，git 就是历史
数据库——这个建模对科研很漂亮，但它需要 §3.1 的 store 才能存在。
而 prux 侧已有三样重叠的东西：会话树的 `/tree`、`rewind` 扩展、子代理的 worktree 隔离。
再加一层「实验树」会让用户面对四套「回到之前某状态」的概念。

### 3.3 skill 随二进制分发的做法

`include_str!` 嵌 SKILL.md + `orx skill <name>` 打印 + `orx install-skills` 写进各家 skills
目录——prux 的 `skills` 扩展已经是同一模式的实现（`assets/skills` 内嵌 + `agent_dir()` 同步），
不需要第二条分发通路。

### 3.4 `workspace_state.rs` / `browser.rs` / `editors.rs` / `folder_picker.rs`

`workspace_state` 是 GUI 布局状态（tab / pane / 宽度 / 滚动 / 缩放），与科研产物无关；
后三个是宿主能力，用 `open` / `xdg-open` 一行就够（prux 的 `open` 扩展已有）。

### 3.5 `orx up` 的本地 dashboard

axum + SSE + 内嵌 React SPA（`ui/` 44 MB）。prux 的 TUI 与 GUI 是两个模式，
再起一个 Web UI 等于第三种宿主，维护成本与收益不匹配。

### 3.6 账号、compute 云、遥测、自更新

`login` / `orgs` / `feedback` / `telemetry` / `updates` 全部依赖 alphaXiv 自家服务端与
cargo-dist 发布产物。prux 侧的 `update_check` 扩展与 `auth` 子命令已覆盖同类需求。

### 3.7 五家 harness 适配

`src/local/harness/`（Claude Code / Codex / OpenCode / Cursor / Antigravity）是
「把别的 agent 当执行器」。prux 自己就是执行器，没有这一层需求。

---

## 四、落地后的收尾检查（通用清单）

> 本节是**通用清单**，不是完成记录。

1. 新扩展 = 声明 `pub mod papers` + 模块内一个工厂静态（`#[linkme::distributed_slice]`），
   不改 `main.rs`；`priority` 取一个新的数值常量并加 `///`；
2. 注册后**默认禁用**，在 `/extension` 面板可开启；工具延迟暴露，`/extension` 详情页要能看到
   前置条件（先用 `tool_search`）；
3. 所有 `const` / `struct` / 字段 / 工具说明按 AGENTS.md 写中文 `///`；
4. 不在回调里跨 `push` / `retain` 持锁，也不在持锁时回调 `hooks()`；
5. 网络请求统一走 `utils::http`，超时与 UA 用现有设施；
6. 提交前 `cargo fmt` + `cargo clippy` + `cargo test`；同步 `assets/changelog/v.<version>.md`；
7. 外链与端点写进扩展文档时不要写「移植自某项目」，按 prux 自己的能力边界描述。
