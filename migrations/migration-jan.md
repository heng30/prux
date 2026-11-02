# Prux 迁移指南：Jan → prux（候选清单，含两条非扩展项）

> **来源**：`jan`（`https://github.com/janhq/jan`，Tauri + TypeScript 的本地 LLM 桌面客户端；
> 本仓的本地只读检出是 `jan -> /home/blue/wayshot-fork/jan`，检出点
> `b8071a116`（2026-10-30），`src-tauri/Cargo.toml` 声明 `license = "Apache-2.0"`，
> 工作区 `package.json` 的 version 是占位的 `0.0.0`，真实版本由 `src-tauri/tauri.conf.json` 决定，
> 本档不引用它）。
>
> **目标**：`src/extensions/` 下的内置扩展，以及少量 `core/` 落点——
> 不与上游插件移植混在一起；本仓已有 `migration-subagents.md` / `migration-tasks.md` /
> `migration-kiss-v0.0.35.md` / `migration-jcode-v0.94.0.md`。
> **性质**：先冻结「值得移植 / 不值得移植」的判断，再逐条落地；每落地一项，
> 更新路线图状态并在该节末尾补「落地记录」。
>
> **判断口径**：Jan 不是 coding agent，是**聊天客户端**。所以本文档只认三种东西：
> 1. **纯 Rust、与宿主无关、prux 缺**的机制（可以直接搬逻辑）；
> 2. 与 prux 功能同构但**做法更稳**的实现（拿来加固现有代码，不算新功能）；
> 3. 参考资料：Jan 踩过的坑与它选择的边界。
>
> 凡是**要靠 Tauri / React / Node / GUI 事件总线才能存在**的东西，一律不列入候选——
> 不是「暂缓」，是对 Rust TUI 无意义。与 `migration-jcode-v0.94.0.md` 里的
> 「判断口径」三不做的区别：这里多一条「宿主不匹配即淘汰」，因为 Jan 的宿主与 prux 完全不同。

---

## 〇、摘要

### 结论

Jan 与 prux 的**重叠面在框架层，不在功能层**：两边都有 MCP client、技能、系统提示词拼装、
上下文压缩、模型元数据、工具沙箱与权限。Jan 剩下的部分几乎全是桌面壳
（`web-app/`、`extensions/*` 的 TS 薄壳、`tauri-plugin-*` 的 `#[tauri::command]` 层、
`packages/adk`、`autoqa`、`protocol`），对终端 agent 不可搬。

按上述口径过筛后：

- **值得移植（2 项，都不是新扩展）**：
  下载资产的**哈希校验 + 断点续传**（§2.1，落 `core/tools_manager.rs`）、
  provider 拒绝后的**反应式压缩重试**（§2.2，落 `core/compaction` + agent 循环）；
- **明确不做（8 项）**：本地文档 RAG / 向量库（§3.1）、本地 gguf·MLX 推理引擎（§3.2）、
  每模型 `model.yml` + router preset（§3.3）、FTS5 记忆检索（§3.4）、
  工具调用沙箱三件套（§3.5）、模型能力元数据表（§3.6）、
  桌面/账号/更新/遥测（§3.7）、Jan 的扩展宿主框架本身（§3.8）。

### 路线图

| # | 项 | 类型 | 落点 | 状态 |
|---|---|---|---|---|
| 2.1 | 下载哈希校验 + 断点续传 | 加固 | `src/core/tools_manager.rs` | 未落地 |
| 2.2 | 反应式压缩重试 | 加固 | `src/core/compaction/` + agent 循环 | 未落地 |

---

## 一、Jan 的结构，以及为什么大半不能搬

| 位置 | 内容 | 对 prux 的意义 |
|---|---|---|
| `web-app/` | React 前端（聊天、设置、模型市场） | 无。prux 的 UI 是 `modes/tui` 与 `modes/gui` |
| `core/src/` | 仅**扩展宿主框架**（`core.ts` / `extension.ts` / `events.ts` / `settings.ts`）：抽象类、事件总线、设置读写 | 仅参考。prux 的宿主是 `core::extensions` + `linkme` 静态注册，机制不同 |
| `extensions/*` | 7 个扩展，**全是转发薄壳**（`invoke(...)` + 事件监听），真逻辑在 `src-tauri/` | 无。夹心层是 Tauri IPC 的产物 |
| `src-tauri/src/core/` | 真逻辑：`agent/`（prompt / skills / compaction / tools）、`mcp/`、`downloads/`、`cli/` | **本档唯一的候选来源** |
| `src-tauri/plugins/tauri-plugin-*` | llamacpp / mlx / vector-db / rag / agent-tools / hardware / websearch | 逐个见 §3 |
| `packages/adk`、`autoqa`、`protocol`、`e2e` | Agent Development Kit、自动 QA、协议定义、端到端测试 | 无 |

一句话：**Jan 的 TypeScript 那一半是宿主适配层，Rust 那一半才有逻辑**；判断候选时只看
`src-tauri/` 里不碰 `AppHandle` / `tauri::command` / `emit` 的模块。

---

## 二、值得移植

### 2.1 下载资产的哈希校验 + 断点续传（落 `core/tools_manager.rs`）

**Jan 的做法**（`src-tauri/src/core/downloads/helpers.rs`）：
- 下载走 `_get_maybe_resume_with_fallback`：先带 `Range` / `If-Range` 继续本地半成品，
  服务端不支持续传就退化为整份重下；
- 下载完 `compute_file_sha256_with_cancellation` 边算边可取消，与清单里的期望值比对；
- `DownloadProgress` 把「下载字节」与「校验字节」两个阶段聚合成一个进度，UI 只画一条。

**prux 现状**：`core/tools_manager.rs` 从 GitHub Releases 拉 `fd` / `rg` 的预编译二进制
（`download_file` / `download_tool`），**只校验 HTTP 状态码，没有哈希校验，也没有续传**。
这是本档唯一一条与安全直接相关的差距：一个被替换的 release asset 会被直接装进用户
`agent_dir()/bin` 并被执行。

**做法**（尽量少写代码）：
1. 哈希**不自建清单**：GitHub Releases API 的 asset 对象已带 `digest`（`sha256:…`），
   `tools_manager` 反正要查 `/releases/latest`，顺手取来比对即可；缺失 `digest` 时保持现状并
   记一条 warn，不要因为拿不到哈希就拒绝安装。
2. 落盘顺序要改：现在 `download_file` 是 `resp.bytes()` 之后直接 `std::fs::write(dest)` 写**最终
   路径**，中途失败会留下半个二进制。改成先写 `<dest>.part`、校验通过再 `rename`，
   让「可执行文件出现」成为校验通过的证据。
3. 续传：只在「`.part` 存在且 `ETag` 与上次一致」时发 `Range`；一致不了的场景一律整份重下——
   唯一必须保住的性质是**校验通过前不暴露可执行文件**。

**验证**：`tools_manager` 的既有单测 + 一个本地 HTTP fixture（起在随机端口，分别返回
「支持 Range」「不支持 Range」「`digest` 不匹配」「无 `digest`」四种响应）；
不联网。

### 2.2 provider 拒绝后的反应式压缩重试（落 core，不是扩展）

**Jan 的做法**（`src-tauri/src/core/agent/compaction.rs` 顶部注释与
`compact_conversation` 附近）：压缩分两层——
1. **主动层**：请求前按预算判断要不要压（prux 已有这一层：`should_compact` /
   `find_compaction_cut_index` / `DEFAULT_KEEP_RECENT_TOKENS`）；
2. **反应式层**：provider **拒绝**请求（上下文超限）时，强制压缩一次再重试一次；
   注释里还记了一条：反应式路径上的 token 消耗要**照常计费**，且摘要为空时要落到 run 自己
   的模型上重试一次，而不是静默丢掉。

**prux 现状**：`core/compaction` 只有主动层。上下文估算偏乐观（不同 provider 的
system prompt 与工具声明开销不同）时，唯一结局是这次请求失败、用户手动 `/compact` 再重发。

**做法**：
1. 在 provider 错误分类里给出「这是上下文超限」的判定（不要按 HTTP 400 泛匹配，
   按各 provider 的错误体关键字，且集中在 `core::provider` 的错误类型上）；
2. agent 循环捕获后：强制压缩一次 → 用压缩后的历史重发一次 → 仍失败就按原错误上报；
3. **只重试一次**，并在重试前把已经流式输出给用户的部分作废（与现有中断路径一致）。

**墙**：这一项会碰 `core::provider` 与 agent 循环，属于「先改契约再改实现」的改动，
动手前先读 `migration-v1.4.0.md` 里与错误分类相关的记录，别把反应式重试塞进
一般重试层（会把 `429` 之类的退避逻辑搅在一起）。

---

## 三、明确不做

### 3.1 本地文档 RAG / 向量库

Jan 有三个扩展（`rag-extension` / `vector-db-extension` / `tauri-plugin-rag`），真实现在
`src-tauri/plugins/tauri-plugin-vector-db/src/db.rs`：每个 collection 一个 SQLite 文件，
`files` / `chunks` 表 + 可选的 `chunks_vec` vec0 虚拟表（运行时 `load_extension` 加载
`sqlite-vec`，失败就退化成逐条 `cosine_similarity` 线性扫描）；切块按**字符** 512 / 64 重叠；
embedding 由本地 gguf 的 `/v1/embeddings` 提供；增量只到「按 `file_id` 先删后插」，
无内容哈希、无失效检测。

不做的三条理由：
1. **prux 没有 embedding 栈**（`memory` 扩展的 BM25 是自实现的词法召回，故意的）；
   向量检索的全部价值都建立在一个可靠的 embedding 来源上。
2. 退化到纯词法后，它与 `memory` 的词法召回、`tool_search` 的 BM25、`rg` 直接检索三者重叠，
   收益只剩「自动切块的连通性」，撑不起一个新扩展。
3. `sqlite-vec` 需要运行时 `dlopen` 扩展，与 prux「单二进制、无外部依赖」的取向冲突。

**翻案条件**：prux 有了 provider 侧的 embedding 能力（`core::provider` 里出现
`type = "embedding"` 的模型并且有可用的本地或远端实现）。

### 3.2 本地 gguf / MLX 推理引擎

Jan 把 llama.cpp 编进进程（`tauri-plugin-llamacpp/src/engine/`，锁 `PINNED_TAG`），
MLX 是 Swift 子进程（`mlx-server/`，随机端口 + 等 ready + SIGTERM→5s→SIGKILL）。

不做的理由：这是**新 provider 后端**，要碰 `core::provider`、显存估算、mmproj 多模态、
模型下载格式解析与构建期 C++ 编译，扩展层完全够不到；且 prux 已有的 `proxy` 扩展反方向
提供了 OpenAI 兼容网关，用户完全可以用 Ollama / llama.cpp 自己起服务再接进来。
代价（构建期编译 C++、三平台打包）与收益（省一次进程边界）完全不匹配。

### 3.3 每模型 `model.yml` + `router.preset.ini`

Jan 为每个本地模型维护一份 `model.yml`（`<providerPath>/models/<id>/model.yml`，含
`model_path` / `mmproj_path` / `size_bytes` / `sha256` / `template_kwargs` / `ctx_size` /
`n_gpu_layers` / `cache_type_k` 等），并由扩展生成仅含非默认值的 `router.preset.ini`。

prux 的等价物是 `assets/models/**` + `make sync-models`（由 `SUPPORTED_PROVIDERS` 决定范围）
与 `model_resolver`。差异是「每模型一份可编辑文件」vs「一份生成产物」，前者只在
**本地模型带私有元数据**时才有意义（`mmproj`、量化等级、KV cache 类型），而 prux 不跑本地模型。

### 3.4 FTS5 记忆检索

Jan 在同一个 SQLite 里维护 `memory_*` 表（FTS5 / BM25）做项目级记忆。prux 的 `memory`
扩展已有自己的词法打分与图结构（`src/extensions/memory/{graph,recall}.rs`）。
换成 SQLite 不会带来新能力，只会多一条持久化路径与一次迁移。

### 3.5 工具调用沙箱三件套

`tauri-plugin-agent-tools` 里的 `sandbox` / `jail` / `gate` 是一套 **CAS（内容寻址）式的
文件权限模型**，与 prux `permissions` 扩展的「四层 pattern + most-restrictive-wins +
`ask` 按 fail-closed」是两套不同的哲学。并存会让用户面对两套规则、两套报错文案。
除非 `permissions` 被证明模型表达力不够，否则不引入第二套。

### 3.6 模型能力元数据表

`src-tauri/src/cli/model_capabilities.rs` 是**手维护**的模型家族上下文表，覆盖顺序
`configured > provider > catalog > fallback(128k)`。prux 的 `assets/models/**` 已经是
provider 侧同步来的权威数据，再叠一张手表是纯负债。
**唯一值得记住的是覆盖顺序本身**：`configured` 最优先、`fallback` 不可静默覆盖用户配置。
这条已体现在 prux 的模型解析里，不需要额外代码。

### 3.7 桌面壳与账号面

`web-app/`、`src-tauri/capabilities/`、updater、`protocol/`、遥测与账号登录：全是 GUI /
发布管道 / 服务端配套。prux 对应的是 `modes/tui` 与 `modes/gui`、`update_check` 扩展、
`auth` 子命令。

### 3.8 Jan 的扩展宿主框架

Jan 的扩展是**运行时加载的 TS 模块**（`core/src/extension.ts` 抽象类 + 事件总线 + 设置存储），
prux 的是**编译期 `linkme` 静态注册 + 优先级排序**。两边的注册语义（禁用到启用、面板条目、
同名命令先到先得）都已经在 prux 侧定死，把 Jan 的框架搬过来等于重写宿主，没有任何功能收益。

---

## 四、落地后的收尾检查（通用清单）

> 本节是**通用清单**，不是完成记录。上面两项都还没落地。

1. 新逻辑要有能证明它生效的最小测试（下载：四种 fixture 响应；压缩重试：mock provider
   返回上下文超限）；
2. 涉及 `core/` 的改动先跑 `cargo test`，并确认 `tests/` 里与 agent 循环相关的用例没退化；
3. 提交前 `cargo fmt` + `cargo clippy`；
4. 用户可见行为（新增的报错文案、`/download` 失败提示）要对齐现有文案风格，
   并在 `assets/changelog/v.<version>.md` 记一行；
5. 若引入新依赖（不应该），先确认它不是为几行代码付的代价。
