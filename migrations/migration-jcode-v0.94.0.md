# Prux 迁移指南：jcode → prux 扩展（候选清单）

> **来源**：`jcode`（`https://github.com/1jehuang/jcode`，Rust 写的终端 coding agent；
> 本仓的本地只读检出是 `jcode -> /home/blue/wayshot-fork/jcode`，检出点
> `2a9e7f624`（2026-10-10），`[package] version = "0.94.0"`（`Cargo.toml:3`））。
>
> **目标**：`src/extensions/` 下的内置扩展（不与上游插件移植混在一起；本仓已有
> `migration-subagents.md` / `migration-tasks.md` / `migration-kiss-v0.0.35.md`）。
> **性质**：先冻结「值得移植 / 不值得移植」的判断，再逐条落地；每落地一项，更新路线图状态
> 并在该节末尾补一块「落地记录」。
> **本档的落地记录按项累积**——第五节「落地后的收尾检查」是**通用清单**，
> 不是完成记录（kiss 迁移档首版曾把未落地的项记成「实施记录」，本档不重蹈）。
>
> **判断口径（本档的尺子）**：一个东西算「可作为扩展」，当且仅当它是**一项独立功能、
> 自带用户可见价值**。满足以下任一条的**一律明确不做**：
> 1. 它是**机制 / 策略**而不是功能——`hooks` 是扩展总线的一环、`command-risk` 是
>    core 的安全策略，都不该另起一个扩展；
> 2. 它是**某个已有扩展的辅助项**——应该并进那个扩展，而不是新起一个
>    （`task-policy` 属 `tasks`、`notify-email` 属 `notify`、子代理文件冲突感知属 `subagent`，
>    三者一律不做）；
> 3. 它需要**服务端 / 守护进程 / 新运行模式 / 新 provider 适配**——扩展层够不到。
>
> **与既有迁移文档的关系**：jcode 与 prux 是**同源重写**（都对齐 pi 契约的 coding agent），
> 所以本文档记的不是「某个上游插件的映射」，而是「同一类工具在两套 Rust 实现之间的
> 功能差集」。凡是 pi 契约本身的差异，以 prux 侧的 `migration-v*.md` 为准。
> 与 `migration-kiss-v0.0.35.md` 的重复项（多账号切换、跨 agent 会话恢复等）沿用那份的
> 结论，只在本文档给出「jcode 侧的额外证据」。

---

## 〇、摘要

### 结论

jcode 与 prux 的重叠面与 kiss 类似：**自动化与 provider 那一半基本已对齐**（子代理、动态
工作流、定时作业、QuickJS 沙箱、MCP、联网检索、技能与提示词模板、压缩、缓存保温、
本地 OpenAI 兼容网关、终端图形协议、mermaid 渲染），对照见 §2。

按 §头部「判断口径」逐项过筛后，jcode 多出来、**且能作为独立扩展**的功能共 **10 项**（§3）：
其中 7 项已落地（`browser`、`pdf`、`open`、`cache-watch`、`session-search`、`usage`、`memory`）、
3 项（`agent-grep`、`panel`、`bg`）明确不做。
**其余全部明确不做**（§4）：包括 3 项被筛掉的「辅助项 / 机制」、2 项属 core 的策略与机制，
以及 11 项扩展层够不到的能力。

### 路线图

| 类别 | 项 | 状态 |
|---|---|---|
| 可作为扩展 | §3.1 记忆系统 `memory` | ✅ 已落地（2026-10-19，词法召回，见 §4.15） |
| 可作为扩展 | §3.2 跨会话检索 `session-search` | ✅ 已落地（2026-10-11） |
| 不做 | §3.3 结构感知检索 `agent-grep` | 🚫 明确不做（2026-10-19） |
| 可作为扩展 | §3.4 provider 配额用量 `usage` | ✅ 已落地（2026-10-19，翻案） |
| 不做 | §3.5 侧面板工具 `panel` | 🚫 明确不做（2026-10-19） |
| 可作为扩展 | §3.6 PDF 文本读取 `pdf` | ✅ 已落地（2026-10-11，`pdf-extract` 0.12） |
| 不做 | §3.7 后台长任务 `bg` | 🚫 明确不做（2026-10-11） |
| 可作为扩展 | §3.8 外部程序打开 `open` | ✅ 已落地（2026-10-11） |
| 可作为扩展 | §3.9 缓存冷警告 `cache-watch` | ✅ 已落地（2026-10-11） |
| 可作为扩展 | §3.10 浏览器自动化 `browser` | ✅ 已落地（2026-10-11，CDP 直连） |
| 不做 | §4.1 破坏性命令门禁 `command-risk` | 🚫 属 core 安全策略 |
| 不做 | §4.2 用户可配置生命周期 hooks `hooks` | 🚫 属 core 扩展机制 |
| 不做 | §4.3 任务质量门 `task-policy` | 🚫 属 `tasks` 扩展的辅助项 |
| 不做 | §4.4 邮件通知渠道 | 🚫 属 `notify` 扩展的渠道 |
| 不做 | §4.5 子代理文件冲突感知 | 🚫 属 `subagent` 扩展的辅助项 |
| 不做 | §4.6–§4.16 其余十一项 | 🚫 见各节 |

---

## 一、架构对照

### 1.1 两仓的模块形状

| 维度 | jcode | prux |
|---|---|---|
| 顶层 | workspace，约 90 个 `crates/jcode-*`（`Cargo.toml:6` 起） | 单 crate（`src/` 分层，见 `AGENTS.md`） |
| 运行形态 | **单 server 多 client**：Unix socket，server 持有全部 session（`docs/SERVER_ARCHITECTURE.md`） | 单进程 TUI；无服务端 |
| 扩展体系 | **无**：功能编译进 crate（`jcode-app-core` 是「业务功能层」） | `src/extensions/` + `linkme` 静态注册 + `Extension` trait（24 个内置扩展，`src/extensions.rs`） |
| 工具注册 | 集中式 `Registry`（`crates/jcode-app-core/src/tool/mod.rs:499`，`base_tools` 在 `:369`） | `src/core/tools/index.rs` 的内置工具 + 各扩展 `tools()` |
| 用户可配置门禁 | `[hooks]`（shell 钩子）+ `command-risk`（内置策略）+ 实验性 safety system | 仅扩展 hook（Rust 内实现），无外部 shell 钩子 |
| 对外接口 | `jcode-harness-api`（版本化 NDJSON）+ `jcode-protocol` + `jcode-sdk` + gateway | 无（仅 MCP 客户端 + 扩展间事件总线） |

**结论**：jcode 的「编译进 crate」与 prux 的「扩展注册表」是两种组织方式，但这不影响
**功能本身能否用 prux 扩展表达**——判断依据是 §1.3 的能力边界，而不是目录形状。

### 1.2 工具面差异

jcode 的模型可见工具（`crates/jcode-app-core/src/tool/mod.rs:369-540` 的注册表）：

`read` / `write` / `agentgrep` / `side_panel` / `panel` / `applet` / `edit` / `replace` /
`apply_patch` / `ls` / `bash` / `compile_remote` / `browser` / `open` / `webfetch` /
`websearch` / `invalid` / `maintainer_feedback` / `jcode_docs` / `todo` / `bg` /
`session_search` / `memory` / `gmail` / `calendar` / `schedule` / `selfdev` /
`desktop_selfdev`，会话级再加 `skill_manage` / `swarm` / `batch` / `conversation_search` /
`codemode` / `integration_tools`（macOS 另有 `macos_computer_use`）。

prux 内置工具只有 7 个：`read` / `bash` / `edit` / `write` / `grep` / `find` / `ls`
（`src/core/tools/index.rs:77`；Windows 另有 `powershell`）。其余能力都在扩展里
（`subagent` 的 `Agent` / `SubagentWorkflow`、`tasks` 的 7 个任务工具、`mcp` 的 `mcp`、
`web-access` 的 `web_search` / `fetch_content`、`codemode`、`goal` 等）。

**差异集中在**：jcode 的 `agentgrep` / `session_search` / `memory` / `bg` / `panel` /
`open` / `browser` / `todo` 在 prux 侧没有对应物（`todo` 由 `tasks` 扩展部分覆盖），
`webfetch`/`websearch` 由 `web-access` 覆盖，`batch`/`codemode` 由 `codemode` 覆盖，
`swarm`/`schedule`/`skill_manage` 分别由 `subagent`/`skills` 覆盖。

### 1.3 prux 扩展系统的能力边界（判断可行性的依据）

判「能不能做成扩展」看 `core::extensions::Extension`（`src/core/extensions.rs`，2775 行）：

- **可注册**：工具（`tools()`）、斜杠命令（`commands()`）、CLI flag（`cli_flags()`）、
  快捷键（`keybindings()`）、设置项（`settings()` / `apply_setting()`）、虚拟模型
  （`virtual_models()`）、MCP 声明（`mcp_servers()`）、工具渲染器（`tool_renderers()`）。
- **可画 UI**：`dock_lines()`、`overlay_view()` / `on_overlay_event()`、
  `render_custom_message()`、`request_ui()`。
- **可挂钩子**（`src/core/extensions/hooks.rs:9` 的 `ExtensionHook`）：
  `TransformContext` / `ContextWithSystem` / `BeforeToolCall` / `AfterToolCall` /
  `AgentEvent` / `Dock` / `Overlay` / `UserPrompt` / `ExtensionEvent` / `Suggestions` /
  `Boundary` / `CacheWarming`。
- **可拦截工具调用**：`before_tool_call_ext` 返回 `BeforeToolCallOutcome`
  （`src/core/extensions.rs:895`：`block` / `reason` / `terminate` / `args`）；
  `after_tool_call_ext` 返回 `AfterToolCallOutcome`（`:920`：可改写文本/content/details/
  isError/usage/terminate）。
- **可持久化**：写会话内 `Entry::Custom`，或自写 `agent_dir()` 下的文件
  （`hang_detect` / `subagent::schedule` / `notify` 的做法）。
- **可跑外部命令**：扩展是进程内 Rust，`notify` 已按空白切词 `spawn` 外部命令。
- **不能**：改内置工具实现（扩展是第二层，只能**新增**工具或用 `after_tool_call_ext`
  改写内置工具的输出）；接管进程 stdio / 监听端口 / 生命周期；新增 provider 适配；
  渲染路径必须同步、无 I/O；`provider_stream_event` / `on_agent_event` 高频且禁止持锁。

**关键推论**：需要**服务端 / 守护进程 / 新运行模式**的能力（swarm、ambient、harness-api、
self-dev、浏览器 bridge 的常驻进程）扩展层够不到；其余都能落在扩展里。

---

## 二、prux 已覆盖（**不需要移植**）

| jcode 能力 | jcode 落点 | prux 对应 | 结论 |
|---|---|---|---|
| 子代理（spawn / stop / resume / 并行池） | `crates/jcode-app-core/src/tool/communicate.rs`（3490 行）+ `server/comm_session.rs` | `extensions/subagent/`（`Agent` / `get_subagent_result` / `steer_subagent` + resume + 并发池） | ✅ 已有（prux 侧在扩展里） |
| 动态工作流（模型写脚本编排） | 无独立工作流引擎（swarm DAG 是另一回事） | `extensions/subagent/workflow/`（QuickJS 沙箱 + `pipeline`/`parallel`/`gate`/`resume`/journal） | ✅ prux 独有 |
| 定时 / 循环作业 | `tool/ambient.rs` 的 `schedule_ambient` + `[agents]` 配置 | `extensions/subagent/schedule.rs`（6 段 cron/间隔/一次性）+ `Agent` 的 `schedule` 参数 | ✅ 已有 |
| 一次脚本编排的作业视图 | `jcode-tui` 的 workflow 视图 | `extensions/subagent/workflow/{progress,inspector,task}.rs` | ✅ 已有 |
| 任务/待办 | `tool/todo.rs` + `jcode-task-types` + `jcode-todo-policy` | `extensions/tasks/`（7 个任务工具 + widget + `/tasks` + 系统提醒注入） | ✅ 已有（催办策略差异见 §4.3） |
| 计划模式 | 无独立 plan mode（`jcode-plan` 是 swarm 级 DAG） | `extensions/plan_mode.rs`（只读探索 + `[DONE:n]`） | ✅ 已有 |
| MCP（本地/远程/OAuth） | `tool/mcp.rs` | `extensions/mcp/` | ✅ 已有 |
| 联网检索与网页回读 | `tool/webfetch.rs` / `websearch.rs` | `extensions/web_access/` | ✅ 已有 |
| 脚本沙箱执行 | `tool/codemode.rs`（opt-in） | `extensions/codemode/`（QuickJS + `ALL_TOOLS`） | ✅ 已有 |
| 并行多工具调用 | `tool/batch.rs`（`MAX_PARALLEL=10`） | `extensions/codemode/`（脚本编排，是超集） | ✅ 等价（机制不同） |
| 技能 / 提示词模板 / 上下文文件 / 项目信任 | `tool/skill.rs` + `jcode-base` 的 skills/prompts | `core/{skills,prompt_templates,context}.rs`、`core/project_trust.rs`、`extensions/skills.rs` | ✅ 已有（语义自动注入见 §3.1） |
| 缓存保温（off/idle） | `jcode-base` 的 cache warming | `core/cache_warmer.rs` + `Extension::cache_warming_decision` | ✅ 已有（冷警告见 §3.9） |
| 自定义 OpenAI 兼容 provider | `openai-compatible` profile + `jcode provider add` | `extensions/proxy/`（本地兼容网关）+ `models.json` | ✅ 等价（机制不同） |
| 循环 / 假死检测 | 无（jcode 靠用户） | `extensions/loop_detect/`、`extensions/hang_detect/` | ✅ prux 独有 |
| 会话导出 HTML / 分支 / 克隆 / 树 | `jcode` 的 export / session picker | `core/export_html*`、`/fork` `/clone` `/tree` `/import` | ✅ 已有 |
| 终端图片 | `crates/jcode-terminal-image/`（Kitty→iTerm2→Sixel，`display.rs:24-77`） | `utils/terminal_image.rs`（kitty/iterm2/sixel/halfblocks + tmux sixel 探测） | ✅ 已有 |
| Mermaid 渲染 | `crates/jcode-tui-mermaid/`（依赖外部 `mermaid-rs-renderer`） | `src/vendor/mermaid_text/`（内嵌 `mermaid-text` 0.57.0） | ✅ 已有 |
| 模糊匹配 | `crates/jcode-fuzzy/`（774 行） | `utils/fuzzy.rs` | ✅ 已有 |
| provider 可达性诊断（离线档） | `jcode-provider-doctor` 的 `Offline` 档（`provider_e2e.rs:44`） | `extensions/doctor.rs`（`HEAD` 探测 + environment 段） | ✅ 已有（深档见 §4.16） |
| 自动更新检查 | `jcode-update-core` | `extensions/update_check.rs` | ✅ 已有（真实自更新已判不做，见 kiss 档 §3.6） |
| 会话内持久记忆（**不是**语义记忆） | `tool/memory.rs` 的显式工具 | `extensions/subagent/memory.rs`（per-agent `MEMORY.md`，200 行上限） | ⚠️ 形状相似、语义不同（见 §3.1） |

---

## 三、能够作为扩展的功能（10 项独立扩展）

### 3.1 语义记忆系统（`memory`）

**功能：** 让 agent 记住「跨会话仍然成立的事」。分三步，都不需要用户手动整理：

1. **抽取**：每跑若干轮（+ 会话结束时）用一次额外的小请求，把对话里的**事实 / 偏好 / 纠正 /
   实体**抽成结构化条目（带 category、置信度、可信度），落盘成一份**记忆图**（JSON，全局一份
   加每个项目一份）——「用户偏好 pnpm」「这个仓库用 `make check` 而不是 `cargo test`」这类；
2. **召回**：之后每次调用模型前，按当前对话的相关性从图里挑出少量条目，作为一段稳定的文本
   注入上下文（有字符/条数预算），所以第二天的会话也不必重新交代一遍；
3. **工具**：另给模型一个 `memory` 工具主动读写（记住 / 回忆 / 搜索 / 列出 / 忘记 / 打标签 /
   建关联），用户可以显式要求「记住这件事」或「忘掉它」。

它与 `subagent` 的 per-agent `MEMORY.md` 的区别：那个是**用户/开发者手写**的说明文件，
按作用域整份注入；这个是**agent 自动抽取 + 按相关性挑选**，且带条目级治理
（覆盖 `superseded_by`、置信度、忘记入口）。风险是记忆污染（错误记忆长期生效），
所以必须有「忘记」与置信度这两条闸门。

| 维度 | 内容 |
|---|---|
| 一句话 | 把对话里的事实/偏好/纠正抽成**记忆图**，之后每轮按相关性召回并注入上下文；另给模型 `memory` 工具主动读写 |
| jcode 出处 | `docs/MEMORY_ARCHITECTURE.md`；存储类型 `crates/jcode-memory-types/src/{lib.rs 1007,graph.rs 665}`；召回/抽取 `crates/jcode-base/src/` 的 `memory.rs`(1641) / `memory_agent.rs`(421) / `memory_jev.rs`(265) / `sidecar.rs`(1715)；工具 `crates/jcode-app-core/src/tool/memory.rs`(802)；嵌入 `crates/jcode-embedding/src/lib.rs`(635) |
| jcode 语义（实测） | 图格式 JSON v2 `MemoryGraph{memories,tags,clusters,edges,reverse_edges,metadata}`（`graph.rs:12,275`），落盘 `~/.jcode/memory/global.json` 与 `projects/<hash>.json`（`memory.rs:173-181,350-355`）；条目字段含 `embedding`、category（Fact/Preference/Entity/Correction/Custom）、trust、strength、`superseded_by`、`reinforcements`、`confidence`（`memory-types/src/lib.rs:224-260`）。**注意 README 与代码不一致**：README.md:311-320 仍写「每轮嵌入 + 余弦」，但 `docs/MEMORY_ARCHITECTURE.md`（2026-09-19）与代码已切到 **Jev 类型化决策召回**（`memory_jev.rs:20`，非向量库），余弦只留在遗留 `find_similar`/hybrid 路径上；词法 BM25 预筛 `MAX_JEV_RECALL_CANDIDATES=72`（`memory.rs:1546-1556`）。本地嵌入 `all-MiniLM-L6-v2`（384 维，ONNX/tract，`jcode-embedding/src/lib.rs:5,58-60`）**默认不编入**（`embeddings` feature opt-in，`Cargo.toml:210-227`）。抽取由 `memory_agent.rs` 每 `PERIODIC_EXTRACTION_INTERVAL=12` 轮 + 会话结束触发（`memory_agent.rs:15,245,367-400`），走可选 LLM sidecar（`sidecar.rs`）。`memory` 工具动作 `remember/recall/search/list/forget/tag/link/related`（`tool/memory.rs:94,109`）。注入预算 `MEMORY_CONTEXT_MAX_CHARS=8000` / `MAX_MESSAGES=12`（`memory_prompt.rs:1-5`）。技能也以合成记忆条目参与召回（`skill.rs:940-965`） |
| prux 现状 | **无语义记忆**。`extensions/subagent/memory.rs` 是 per-agent `MEMORY.md` 文件注入（user/project/local 三作用域），与「自动抽取 + 相关性召回」不是一回事 |
| prux 落点 | 扩展 `memory`：存储 `agent_dir()/extensions/memory/<project-hash>.json`（JSON 图，照 jcode 形状）；`tools()` 提供 `memory`（`remember`/`recall`/`search`/`list`/`forget`/`link`/`related`）；`TransformContext`（或 `ContextWithSystem`）按相关性注入；`Boundary`（`turn_end`）推进抽取 cadence；`on_session_start` 载入；`settings()` 暴露开关与预算。**召回范围**：先做**词法**（BM25/子串）——prux 无 embedding 栈（§4.15），jcode 自己的自动召回也已不依赖余弦 |
| 接缝与约束 | 注入会**打掉 prompt cache**：`transform_context` 每次 LLM 调用都跑，必须只在「召回结果变化时」才追加，且注入块要稳定（否则每轮 cache miss）。抽取是**额外一次模型请求**，要走一次性请求路径（照 `recap`/`btw` 的 `shared::spawn_one_shot`），不能塞进主回合；cadence 与去重要落进扩展 state。`overlay_view` 每帧调用且无 I/O，记忆图浏览面板只画缓存 |
| 风险 | 记忆污染（错误记忆长期生效）需要 `superseded_by`/`confidence` 与「忘记」入口；注入预算失控会吃上下文；隐私——记忆落盘在 `agent_dir()`，要写进文档 |
| 验证 | 图读写往返、去重/强化、BM25 召回排序、注入块的稳定性（同一召回两次产生相同文本）、cadence（K 轮触发一次抽取）、`memory` 工具各动作、预算截断；`AgentDirGuard::temp()` 隔离目录 |
| 工作量 | **大**（jcode 相关代码数千行；prux 首版可只做「抽取 + 词法召回 + 工具」三件套） |
| 默认状态 | 默认不启用（`Dev` / `Creator`） |

> **落地记录（2026-10-19）**：落点 `src/extensions/memory.rs` +
> `src/extensions/memory/{graph,store,config,recall,extract}.rs`。
> 按本档建议只做**词法**召回（无 embedding，§4.15），并保留「抽取 + 召回 + 工具」三件套。
>
> - **存储**：`agent_dir()/extensions/memory/global.json`（跨项目）与
>   `.../projects/<project-key>.json`（按工作目录；键复用 `extensions::util::project_key`，
>   与会话目录同名）。JSON 图沿用 jcode 的 v2 形状（`graph_version` / `memories` / `tags` /
>   `clusters` / `edges` / `reverse_edges` / `metadata`），但**去掉 embedding 字段**、
>   `clusters` 恒空（prux 不做聚类）；单份存储上限 1000 条，超出按打分淘汰。
> - **召回**：`ContextWithSystem` 钩子，用**当前用户消息**做 BM25（词元来自
>   `utils::text::terms`，中日韩按二字组合展开），命中条目按分类拼块追加到本次请求。
>   块内容确定性（分数 → 更新时间 → id 排序），且按「查询文本」缓存，一轮之内只算一次——
>   注入一变就会打掉 prompt cache。无命中的查询不注入任何东西。
> - **抽取**：`Boundary` 钩子的 `turn_end` 推进 cadence（默认每 12 次），
>   `agent_before_settle` 用 `contextMessages` 拼摘录、起一次独立的一次性请求
>   （`auth::ensure_oauth_valid` → `model_resolver::configured_model` →
>   `provider::simple_completion`），回复按 JSON 数组解析（容忍代码围栏与前后说明文字），
>   同分类同正文视为强化、否则新增。`--offline` 跳过。
> - **工具**：`memory`（`remember` / `recall` / `search` / `list` / `forget` / `tag` / `link` /
>   `related`），声明为**直接工具**（`ToolExposure::Direct`）。
>   曾声明 `Deferred` + 前置 `tool_search`，后因 `remember` / `forget` 是 push 型动因而改回直接：
>   延迟后 `snippet` / `prompt_guidelines` 在激活前不进系统提示，而驱动模型主动写记忆的正是那条
>   guideline，空记忆冷启动时召回块也不出现，模型手里没有任何信号；省下的四百余字符比召回
>   注入块（默认上限 8000 字符）小一个数量级。
>   相比本档原先的清单补了 `supersedes`（`remember` 时取代旧条目，走
>   `superseded_by`）与 `tag` 的 `-name` 删除语法；`forget` 是真删（对齐 jcode），
>   `supersede` 才是保留追溯的失效。
> - **配置**：`agent_dir()/extensions/memory.json` 的 `recallLimit` / `extractInterval` /
>   `injectBudgetChars`（`0` 分别表示关闭自动召回 / 自动抽取 / 注入），`/memory settings` 面板循环切换。
> - **命令**：`/memory status|list|search <q>|forget <id>|settings`。
>
> **与本节原计划的差异**：
> - 原计划写「`TransformContext`（或 `ContextWithSystem`）」——选了后者：它结果原样发送、
>   不改写 `agent.messages`，所以注入不会进会话历史、也不会被后续压缩当成真实对话
>   （与 `tasks` 的提醒注入同一取舍）；
> - 原计划的「会话结束时抽取」没有独立钩子可用，落到整轮结算（`agent_before_settle`），
>   与周期性抽取共用同一条路径；
> - 原计划让 `periodic extraction` 每 K **轮**触发：实现按 `turn_end` 计数（多轮 run 与
>   单轮 run 一视同仁），间隔由设置项控制；
> - 显式 `recall` / `search` 会给返回条目累加访问次数并落盘（`access_count`/置信度回升），
>   **自动注入不写盘**——那条路径在每次请求前跑，不能带 I/O。
>
> 验证：`cargo test --lib extensions::memory`（58 个用例：图读写往返与容量淘汰、标签/边/取代、
> 时间衰减与访问加成、BM25 排序与 CJK bigram、注入块的确定性与预算截断、
> 抽取回复解析（代码围栏 / 字符串里的方括号 / 别名分类 / 去重 / 条数上限）、
> cadence 推进、工具八个动作、项目/全局隔离与缺 cwd 报错、`/memory` 各子命令）。
> 声明 `fork_project()` 指向 jcode 0.94.0（来源版本取自本档）。
> 默认不启用、`modes() == [Dev, Creator]`；工具当时声明为 `Deferred` + 前置 `tool_search`，
> 该决定已被后续改动推翻（改为 `ToolExposure::Direct`，去掉 `prerequisite_tools()`）。

### 3.2 跨会话检索（`session-search`）

| 维度 | 内容 |
|---|---|
| 一句话 | 一个 `session_search` 工具：在**历史会话**里做 RAG 式检索，按会话分组返回片段与元数据 |
| jcode 出处 | `crates/jcode-app-core/src/tool/session_search.rs`（1892 行）+ `session_search_index.rs`；跨 jcode/claude/codex/pi/opencode/cursor 的加载走 `jcode-import-core` |
| jcode 语义（实测） | 默认隐藏当前会话、system reminder 与纯工具消息；会话元数据是一等结果；连 snapshot + journal 一起搜（近期消息可见）；结果按会话分组避免刷屏（`session_search.rs:1-10`）。检索走二进制 token-hash 索引（`session_search_index.rs`），并可加载外部 harness 的会话（`load_{claude,codex,cursor,opencode,pi}_external_*`，`session_search.rs:24-30`） |
| prux 现状 | 无会话检索工具。只有 `/resume`（选会话）与 `/import <path>`（导入自家 jsonl） |
| prux 落点 | 扩展 `session-search`：`tools()` 提供 `session_search`，扫描 `agent_dir()/sessions/**/*.jsonl`，用 `Session::open_checked`（`src/core/session_manager.rs:715`）解析后按关键词打分；结果按会话分组、隐藏当前会话。可复用 `cache_usage` 的会话扫描骨架（`spawn_blocking` + 最新 200 份上限） |
| 接缝与约束 | 工具执行是正常 I/O，可 `spawn_blocking`；不要在 `overlay_view`/`dock_lines` 里扫盘。**只搜 prux 自家会话**——跨 harness 检索要读别家私有 jsonl，与 §4.10 同一理由不做 |
| 风险 | 大历史下扫描慢（用 mtime 排序 + 上限）；检索质量（无 embedding 时靠词法，够用即可） |
| 验证 | 造多份临时会话 jsonl → 断言关键词命中、按会话分组、隐藏当前会话、上限与排序；目录隔离 `AgentDirGuard::temp()` |
| 工作量 | 中 |
| 默认状态 | 默认不启用（`Dev` / `Creator`） |

> **落地记录（2026-10-11）**：落点 `src/extensions/session_search.rs`（`session_search` 工具，延迟工具 +
> `prerequisite_tools()` 声明 `tool_search`）。只搜 prux 自家会话，跨 harness 检索不做（§4.10）；
> 无 embedding，走词法打分（§4.15）。
>
> **与本节原计划的差异**：
> - 原计划只提「关键词打分 + 按会话分组」，落地时补了三处 jcode 语义：**会话名作为独立结果**
>   （得分 ×2）、**压缩/分支摘要参与检索**、**`<system-reminder>` 注入消息一律跳过**；
> - 原计划的「复用 `cache_usage` 的会话扫描骨架」实际只复用了 `session_manager::list_sessions`
>   与 `encode_cwd_dir` 的思路——`cache_usage` 的骨架绑着面板 UI 与 `Usage` 聚合，与「读全文打分」
>   不是一回事，硬套反而更乱；
> - 新增了两个原计划没有的边界：**单条消息 5 万字符 / 单份会话 400 万字符**的扫描上限，
>   以及**扩展在会话中途被启用**时「当前会话」的退回判定（用「当前 cwd 会话目录里最新那份」近似，
>   并在 `on_enabled_changed(true)` 时清掉可能过期的记录）。
>
> 验证：`cargo test --lib extensions::session_search`（36 个用例：词元切分与停用词拒绝、打分与
>  `MAX_TERM_HITS` 封顶、摘录窗口与 UTF-8 截断、会话读写与摘要/角色归类、当前会话排除、
> 会话名命中、目录/provider/model/时间/role/limit/`max_per_session`/`max_scan_sessions` 各过滤器、
> 坏文件跳过、渲染与无命中文案、端到端工具调用）。
> 声明 `fork_project()` 指向 jcode 0.94.0（来源版本取自本档），供 `/extension` 面板展示来源链接。
> 默认不启用、`modes() == [Dev, Creator]`。

### 3.3 结构感知检索（`agent-grep`）

| 维度 | 内容 |
|---|---|
| 一句话 | grep 返回里附带**文件结构**（函数/区域起止行），并按「agent 已经看过什么」自适应裁剪已见内容，省上下文 |
| jcode 出处 | `crates/jcode-app-core/src/tool/agentgrep.rs`(517) + `agentgrep/args.rs`(278) + `agentgrep/context.rs`(1044)；底层是**外部 crate** `agentgrep v0.1.7`（`crates/jcode-app-core/Cargo.toml:111`），jcode 只做封装 |
| jcode 语义（实测） | 4 种模式 `grep`/`find`/`outline`/`trace`(别名 `smart`)（`agentgrep.rs:218-260`）；`grep` 结果带文件结构（`@ start-end`）。自适应截断只在 `trace`/`outline` 生效：扫当前会话的 `read`/`agentgrep`/`bash` 调用与结果，提取 agent 已看过的 region/file/symbol，赋 `body_confidence`/`current_version_confidence`/`prune_confidence`（`context.rs:85-190`），按消息位置与压缩点做记忆衰减（active_tail=1.0 / recent=0.88 / older=0.72 / compacted=0.42，`context.rs:437-480`），再按文件 mtime 判 `file_changed_since_seen`（`context.rs:483-520`）。参数 `mode/query/regex/path/glob/type/max_files/max_regions/paths_only/hidden/no_ignore/full_region`；`DEFAULT_GREP_MAX_REGIONS=200`（`agentgrep.rs:87`）；前台预算 `AGENTGREP_FOREGROUND_BUDGET=5s`，超时转 `bg`（`:24,301-350`） |
| prux 现状 | `grep` 是内置工具（`src/core/tools/grep.rs`，基于 `ripgrep --json` 流式搜索，299 行），只有 `pattern`/`path`/`glob`/`ignore_case`/`fixed_strings`/`context`/`limit`，**无结构信息、无自适应裁剪** |
| prux 落点 | 两条路：① 新扩展 `agent-grep` 提供 `agentgrep` 工具（prux 扩展只能**新增**工具，不能替换内置 `grep`）；② 扩展用 `after_tool_call_ext` 拦截 `grep`，把结构信息追加到结果文本。**推荐 ①**（不干扰内置 `grep` 的既有语义与测试）。结构提取：prux 无 tree-sitter（`Cargo.toml` 只有 `syntect` 做高亮），首版用轻量启发式（按语言的正则/缩进/大括号匹配函数与区域），或引入 `agentgrep` 依赖（需评估其跨平台与维护状态） |
| 接缝与约束 | `after_tool_call_ext`（`extensions.rs:875`）可改写内置工具输出，但**每次 grep 都要重解析**；自适应裁剪需要跨调用的「已见状态」，存扩展 state（`on_agent_event` 里累积 `read`/`grep` 的命中）。前台 5s 预算要照抄（超时降级为普通 grep 结果，别挂住） |
| 风险 | 结构提取的准确率（启发式会误判）；自适应裁剪误删有用内容——首版可只做「结构信息」不做「裁剪」，把裁剪当第二阶段 |
| 验证 | 结构提取纯函数单测（对样本文件断言函数名与起止行）；裁剪决策单测（已见/未变/已变三态）；工具集成测断言输出含结构段 |
| 工作量 | 中—大（裁剪逻辑复杂；只做结构信息则中等） |
| 默认状态 | 默认不启用（`Dev` / `Creator`） |

> **决定（2026-10-19）**：🚫 **明确不做**。两条路都不划算：
> ① 结构信息的价值靠**准确率**撑着，而 prux 侧没有 tree-sitter，只能自研启发式（按语言写正则/缩进/大括号
> 匹配），误判的函数边界比没有结构信息更坏；引入 `agentgrep` 外部 crate 又等于把一块无人维护的
> 解析器塞进内置扩展（§4.15 拒本地 embedding 是同一套理由）。
> ② 真正有价值的自适应裁剪（`trace`/`outline` 按「agent 是否看过」裁掉已见内容）要复刻 jcode
> 上千行的置信度模型：记忆衰减档位、压缩点感知、文件 mtime 判定、跨调用的已见状态累积——
> 这是「省上下文」的优化，不是缺功能，而 prux 已经有压缩与 `context` 预算两条主线在管上下文。
> 另：prux 的内置 `grep` 是 `ripgrep --json` 流式实现，扩展只能**新增** `agentgrep` 工具
> （模型要在两个 grep 之间自己选）或用 `after_tool_call_ext` 改写内置输出（每次调用重解析）——
> 两条都比「内置 grep 直接变好」差。
> **本档不再把本项列为待落地项**；若将来内置 `grep` 要上结构信息，那是 core 的一次改动，不是扩展。

### 3.4 provider 配额用量（`usage`）

| 维度 | 内容 |
|---|---|
| 一句话 | `/usage` 覆盖面板：逐个已连接 provider 显示**配额/限额用量**与重置时间 |
| jcode 出处 | `crates/jcode-tui-usage-overlay/src/lib.rs`（973 行）；命令入口 `handle_usage_command`（`crates/jcode-tui/src/tui/app/commands.rs:3884`）→ `request_usage_report`（`misc_ui.rs:141`） |
| jcode 语义（实测） | 状态枚举 `Loading/Good/Warning/Critical/Error/Info`（`lib.rs:8-56`），每 provider 渲染用量条 + 重置时间（`provider_detail_lines`，`lib.rs:663+`），支持过滤与键位（`handle_overlay_key`，`lib.rs:329`）。数据来自 provider 的用量/限额端点（OAuth 订阅类 provider 才有）。注意 `docs/MODEL_USAGE.md` 的 `ModelUsage` 台账（存 `~/.jcode/model-usage-v1.sqlite3`）是**另一套**本地计数 |
| prux 现状 | 底栏只显示 token/缓存/费用（`extensions/footer.rs`），没有**配额**视图；`cache-usage` 是历史命中率，不是配额 |
| prux 落点 | 扩展 `usage`：`commands()` 加 `/usage`，`overlay_view` 画面板；数据源按 provider 类型分别取（Anthropic/OpenAI 等的用量端点，需各自适配）——这是本项最大的不确定性。首版可先做**本地台账**（会话 usage 汇总，与 `cache-usage` 同源），配额端点作为增量 |
| 接缝与约束 | `overlay_view` 每帧调用、必须同步无 I/O → 用量要在命令执行时异步拉好、缓存进 state。provider 配额端点未文档化且随 provider 变化，属**长期维护**面（同 §4.10 的逻辑） |
| 风险 | 配额端点不稳定/需凭据；把本地台账误当配额展示会误导用户——要区分标注 |
| 验证 | 本地台账聚合单测（同 `cache_usage`）；配额解析做成可注入的纯函数（假响应断言状态分类与条形文本） |
| 工作量 | 中（本地台账小；配额端点适配是大头） |
| 默认状态 | 默认不启用（`Dev` / `Creator`） |

> **决定（2026-10-19，翻案）**：✅ **改为落地**。上面那条「明确不做」的理由里，
> 「本地台账与 `cache-usage` 完全同源」依然成立（所以首版**不做本地台账**，只做配额端点）；
> 但配额端点这一侧的维护成本是可以收窄的——把适配面限定在 **prux 真实支持的 provider**
> （而不是 jcode 那份含 Cursor / Antigravity / Copilot 多账号的长清单），并且让**每个 provider 的
> 失败只影响它自己那一行**，端点变了也只是某行开始报错，不会拖垮面板。
> 原决定里那条「得先改 core 才能读 `anthropic-ratelimit-*` 响应头」不影响本项：走各家的**配额端点**
> 而不是响应头，纯扩展可完成。

> **落地记录（2026-10-19）**：落点 `src/extensions/usage.rs`（扩展层：命令、面板、状态、缓存）
> + `src/extensions/usage/fetch.rs`（数据层：各 provider 的端点、请求与**纯函数解析**）。
> 命令 `/usage` 打开 Panel 覆盖层。
>
> **覆盖的 provider**（`fetch::SUPPORTED`，只在已配置凭据时才查）：
> `anthropic`（`GET /api/oauth/usage`，5 小时 / 7 天 / 7 天 Opus + `limits[]` 里的模型周窗口 + extra usage）、
> `openai`（`GET chatgpt.com/backend-api/wham/usage`，`rate_limit.primary_window`/`secondary_window`
> + `additional_rate_limits` + 重置券张数；账户 id 从 access token 的 JWT 声明里取，取不到就不带该头）、
> `github-copilot`（`GET copilot_internal/v2/token`，`limited_user_quotas` 的已用/上限 + 重置日期，
> 用 `cred.refresh` 里的 GitHub token 认证，网关头复用 `model_resolver::provider_headers`）、
> `openrouter`（`/api/v1/credits` + `/api/v1/key`：额度条 + 今日/本周/本月花费 + key 限额）、
> `kimi-coding`（`/coding/v1/usages`，新版 `limits[]` 与旧版 `usages` 映射都接受）、
> `zai` / `zai-coding-cn`（`monitor/usage/quota/limit`，按 key 区域选 origin：`api.z.ai` / `open.bigmodel.cn`）、
> `deepseek`（`/user/balance`）与 `moonshotai` / `moonshotai-cn`（`/users/me/balance`）。
> 其余已配置凭据、但没有配额端点的 provider 由面板末行 `no quota endpoint: …` 点名。
>
> **与本节原计划的差异**：
> - 原计划的「首版先做本地台账」**没有做**：它与 `cache-usage` 同源，价值增量小（原决定的第一条理由仍然成立），
>   配额端点才是本项唯一不可替代的部分；
> - 缓存策略从「120 秒内不重复拉」落到 state 的 `fetched_at` + `REPORT_TTL_MS`：TTL 内再执行 `/usage`
>   直接复用报告（`overlay_view` 因此永远只读缓存、无 I/O）；
> - 面板没做 jcode 的过滤/键位（`handle_overlay_key`），只保留 ↑↓ 滚动与 Esc 关闭：prux 的 provider 数
>   远少于 jcode（且只列有端点的），过滤属于没有用户的复杂度；
> - 每个 provider 的解析全部是纯函数（`parse_anthropic` / `parse_openai` / `parse_copilot` /
>   `parse_openrouter_*` / `parse_kimi` / `parse_zai`），网络函数另有 `*_at(base)` 变体，
>   测试用**本地假 HTTP 服务**驱动端到端路径，全程不联网。
>
> 验证：`cargo test --lib extensions::usage`（29 个用例：百分比/重置时间归一化、各 provider 解析、
> 余额与错误渲染、离线跳过、TTL 缓存、陈旧面板丢弃、本地假服务端到端）。
> 声明 `fork_project()` 指向 jcode 0.94.0（来源版本取自本档），供 `/extension` 面板展示来源链接。
> 默认不启用、`modes() == [Dev, Creator]`。
>
> **原决定（2026-10-11，已被上面取代）**：🚫 **明确不做**。两条路都不够干净：
> ① 配额端点（Anthropic 的 `GET /api/oauth/usage`、ChatGPT 的 `backend-api/wham/usage`、
> Copilot 的 `copilot_internal/v2/token`、Cursor 的 `GetCurrentPeriodUsage`、Antigravity 的
> `fetchAvailableModels`、OpenRouter、Kimi、Z.ai 等）要逐 provider 适配、要读 OAuth 凭据、
> 端点未文档化且随 provider 变动，属长期维护面；② 本地台账与 `cache-usage` 完全同源
> （同一份会话 `Usage`），只是把视角从命中率换成花费，信息增量太小。
> 另需注意：prux 的 provider 层目前**不捕获** `anthropic-ratelimit-*` / `x-ratelimit-*` 响应头
> （全仓无引用），所以「从响应头读配额」这条免费且准确的路子得先改 core（provider 层透出响应头），
> 不是纯扩展工作。**本档不再把本项列为待落地项**；若将来要捡起，先定数据源并单独立项。

### 3.5 侧面板工具（`panel`）

| 维度 | 内容 |
|---|---|
| 一句话 | 给模型一个 `panel` 工具，把 Markdown 内容开在侧面板里（`spawn` / `update` / `focus` / `close` / `list`），可当辅助信息区或 diff 查看器 |
| jcode 出处 | `crates/jcode-app-core/src/tool/panel.rs`（139 行，动作 `spawn/update/focus/close/list`）；旧实现 `side_panel.rs`（216 行）；`docs/PANELS.md`（71 行） |
| jcode 语义（实测） | 面板内容可以是 Markdown 文本，或指向 Markdown/PDF 文件；PDF 在桌面面板里有尺寸限制（20 MiB/文件、32 MiB/会话，`PANELS.md:56-64`）。这是**桌面窗口**面板（jcode 有桌面运行时） |
| prux 现状 | 无「模型驱动打开侧面板」的工具。prux 有覆盖面板（`overlay_view`）与 `render_custom_message`，但都由扩展自己触发，模型够不到 |
| prux 落点 | 扩展 `panel`：`tools()` 加 `panel` 工具，动作映射到 `request_ui`（`src/core/extensions/ui.rs:181`）+ `overlay_view`；面板内容是 markdown → 复用 prux 的 markdown 渲染。jcode 是桌面窗口，prux 只能画在**终端覆盖层**（`OverlaySize::Panel` 或全屏），语义要相应收窄 |
| 接缝与约束 | `overlay_view` 每帧调用且必须同步无 I/O → 面板内容在工具执行时算好、缓存在 state。工具调用的结果文本仍要返回给模型（否则模型看不到面板是否开成功） |
| 风险 | 与 `render_custom_message` 的职责重叠（一个是消息卡、一个是常驻面板）；终端覆盖层空间有限，长文档要分页/滚动 |
| 验证 | 面板认领与滚动、工具动作解析（缺参数报错）、`update` 覆盖同 id、`close` 释放；渲染用纯函数断言 |
| 工作量 | 中 |
| 默认状态 | 默认不启用（`Dev` / `Creator`） |

> **决定（2026-10-19）**：🚫 **明确不做**。jcode 的 `panel` 是**桌面窗口**面板（内容可以是
> Markdown/PDF 文件、有尺寸上限），prux 只能把它降级成终端覆盖层——而 prux 已经有两条更贴合
> 终端的能力：`render_custom_message`（模型结果落成聊天区卡片）与 `overlay_view`（扩展自己的
> 常驻面板）。再加一个「模型请求开面板」的工具，真正多出来的只有
> 「模型能主动弹块 Markdown 给用户看」这一件事，而这件事用**普通回答 + 聊天区渲染**已经能做到，
> 代价却是新增一族需要分页/滚动/更新/关闭状态机的 UI 面（还要和 `render_custom_message` 划清职责）。
> **本档不再把本项列为待落地项**；若将来确有「模型驱动的常驻信息区」需求，先定义清楚它与
> 聊天区卡片的分工再单独立项。

### 3.6 PDF 文本读取（`pdf`）

| 维度 | 内容 |
|---|---|
| 一句话 | 让 `read` 能读 PDF 的文本内容（jcode 的 `read` 支持文本/图片/PDF） |
| jcode 出处 | `crates/jcode-pdf/src/lib.rs`（51 行）：用 `pdf_extract` 抽文本 + `catch_unwind` 兜住解析 panic（`:10-22`）；工具侧在 `read` 的 mime 分派里 |
| jcode 语义（实测） | 只抽**文本**，不渲染版面；解析可能 panic，用 `catch_unwind` 恢复后返回错误而不是崩 |
| prux 现状 | `read` 支持文本与图片（`utils/mime.rs` + `read` 工具的图片分支），**不支持 PDF** |
| prux 落点 | 两条路：① 扩展提供 `read_pdf` 工具（prux 扩展只能新增工具）；② 给内置 `read` 加 PDF 的 mime 分支（**core 改动**，能顺带让图片/PDF 走同一分派）。若按本档尺子（不改内置工具），走 ①。依赖：`pdf-extract` 或同类 crate，需评估体积与跨平台 |
| 接缝与约束 | 工具执行是正常 I/O；大 PDF 要有页数/字节上限，截断提示要照 prux 既有 `truncate` 风格 |
| 风险 | 引入新解析依赖（编译体积、C 依赖、跨平台）；扫描版 PDF 抽不出文本（要提示用户，别静默返回空） |
| 验证 | 造小 PDF 断言文本抽取；损坏文件断言返回错误而非 panic；超限截断 |
| 工作量 | 小—中 |
| 默认状态 | 默认不启用（`Dev` / `Creator`） |

> **落地记录（2026-10-11）**：落点 `src/extensions/pdf.rs`（`read_pdf` 工具）。依赖取
> **`pdf-extract` 0.12**（`default-features = false`，带进 `lopdf` / `ttf-parser` / `euclid` 等）；
> 按本档建议走 ①「新起工具」，不动内置 `read`。32 MiB 字节上限在解析前拦下，抽出文本按
> `truncate_head` 的口径截断（2000 行 / 50KB，与 `read` 同式）；文本层为空时明确提示
> 「可能是扫描件」而不是静默返回空；解析器 panic 用 `catch_unwind` 兜住（release 构建是
> `panic = "abort"`，该兜底只在 debug / 测试构建生效）。验证：`cargo test --lib extensions::pdf`
> （11 个用例：路径解析、目录与超大文件拒绝、损坏文件不 panic、手工构造的最小 PDF 抽取往返）。
> 声明 `fork_project()` 指向 jcode 0.94.0（来源版本取自本档），供 `/extension` 面板展示来源链接。
> 默认不启用、`modes() == [Dev, Creator]`。

### 3.7 后台长任务（`bg`）

| 维度 | 内容 |
|---|---|
| 一句话 | 一个 `bg` 工具：把长时间运行的命令放后台，模型能查状态/进度/输出，而不是阻塞等待 |
| jcode 出处 | `crates/jcode-app-core/src/tool/bg.rs` + `crates/jcode-background-types/`（201 行，后台任务状态/进度模型） |
| jcode 语义（实测） | 后台任务有独立的状态与进度模型（`jcode-background-types`），TUI 侧有对应展示；`agentgrep` 的前台预算超时也会转 `bg`（`agentgrep.rs:301-350`） |
| prux 现状 | `bash` 只有同步执行，**无后台任务概念**（`src/core/tools/bash.rs`） |
| prux 落点 | 扩展 `bg`：`tools()` 加 `bg` 工具（start/status/output/stop），在扩展 state 里维护子进程表；`dock_lines` 显示运行中任务。子进程管理复用 `tokio::process` |
| 接缝与约束 | 进程生命周期要跟会话绑定（会话切换/退出时清理），扩展有 `on_session_switched` / `on_enabled_changed` 可挂；`dock_lines` 每帧调用、只读 state |
| 风险 | 孤儿进程（要保证清理）；输出缓冲无上限会吃内存（要有环形缓冲与上限） |
| 验证 | 起一个短命令断言状态流转与输出回读；stop 后断言进程退出；会话切换清理 |
| 工作量 | 中 |
| 默认状态 | 默认不启用（`Dev` / `Creator`） |

> **决定（2026-10-11）**：🚫 **明确不做**。后台任务要接管子进程生命周期、输出缓冲上限与
> 会话切换时的清理，属 core 的 `bash` 工具语义（不是一层可选功能）；本档不再把它列为待落地项。

### 3.8 外部程序打开（`open`）

| 维度 | 内容 |
|---|---|
| 一句话 | 一个 `open` 工具：用系统默认程序打开文件/URL（在用户看得见的窗口里打开，而不是自己读） |
| jcode 出处 | `crates/jcode-app-core/src/tool/open.rs` |
| prux 现状 | 无。prux 只有 `cmd`/`xdg-open` 之外的 TUI 内部打开（无此工具） |
| prux 落点 | 扩展 `open`：`tools()` 加 `open` 工具，按平台调 `xdg-open` / `open` / `start`。极小 |
| 接缝与约束 | 尊重 `--offline`？打开 URL 不需要网络，但要在文档写明它会拉起外部程序 |
| 风险 | 拉起外部程序属副作用，结果文本要说清打开了什么；无终端/无 `xdg-open` 时要报错而不是静默 |
| 验证 | 参数校验与命令拼装单测（不真开程序，用可注入的 runner） |
| 工作量 | 极小 |
| 默认状态 | 默认不启用（`Dev` / `Creator`） |

> **落地记录（2026-10-11）**：落点 `src/extensions/open.rs`（`open` 工具）。参数只有 `path`：
> `http` / `https` / `file` URL 原样透传，其余按 `ctx.cwd` 展开并要求存在；按平台拼
> `xdg-open` / `open` / `cmd /C start`（Windows 的 `start` 是 cmd 内建命令，补一个空串占位当窗口标题）。
> 有 tokio 运行时就把子进程交给后台任务 `wait`（避免僵尸进程），否则退回标准库 `spawn`。
> 验证：`cargo test --lib extensions::open`（9 个用例：URL 判定、路径展开与缺失、平台命令拼装、
> 工具 schema）。声明 `fork_project()` 指向 jcode 0.94.0（来源版本取自本档），供 `/extension` 面板展示来源链接。
> 默认不启用、`modes() == [Dev, Creator]`。

### 3.9 缓存冷警告（`cache-watch`）

| 维度 | 内容 |
|---|---|
| 一句话 | 提示「prompt cache 已冷」（按 TTL 过期）与「本应命中却未命中」的意外 cache miss |
| jcode 出处 | `crates/jcode-tui/src/tui/app.rs:2033`（`push_cold_cache_warning_for_baseline`，按 TTL 过期插入「Prompt cache went cold · …/cache extends」，`:2076-2084`）、`:2472`（`maybe_push_kv_cache_miss_notice`，仅 harness 自因告警，有记录原因时降级为 ℹ️）；分类器 `crates/jcode-base/src/kv_cache_monitor.rs`（390 行，`KvCacheMiss::message` 在 `:135`） |
| jcode 语义（实测） | 冷警告按 TTL（如 Claude 的 5 分钟）算，输入栏还有倒计时（`ui_input.rs:1822`/`1854`）；意外 miss 只报**harness 自因**（system/tools/prefix 变化），用户/模型导致的 miss 不报（受 `features.kv_cache_miss_notices` 控制） |
| prux 现状 | `core/cache_warmer.rs` 只管「要不要保温」（`WarmStop` / `WarmEconomics`）；`extensions/footer.rs` 显示 `cache_read`/`cache_write`/命中率，但**不判冷、不报意外 miss** |
| prux 落点 | 扩展 `cache-watch`：`AgentEvent` 里看 `turn_end` 的 usage 与 `CacheWarming` 钩子的保温结果，比对「应该命中」（上一轮写入且未超 TTL 且前缀未变）与「实际命中」；不一致时推一条系统消息。TTL 可复用 `core/cache_warmer.rs` 的 `prompt_cache_ttl`（`:105`）与 `warming_delay_ms`（`:127`） |
| 接缝与约束 | `provider_stream_event` 高频、禁止 I/O；判定逻辑要纯函数；系统消息注入走 `render_custom_message` 或普通系统消息 |
| 风险 | 误报（前缀判定的近似会错）——首版可只做「冷警告」（TTL 判定确定性强），意外 miss 当第二阶段 |
| 验证 | TTL 判定纯函数单测（各模型 TTL 与边界）；前缀未变/已变的 miss 判定矩阵 |
| 工作量 | 小 |
| 默认状态 | 默认不启用（`Dev` / `Creator`） |

> **落地记录（2026-10-11）**：落点 `src/extensions/cache_watch.rs`（无工具，纯提示）。
> 冷警告挂在 `on_user_submit`（提交新一轮时比对上次写入距目录 TTL），意外未命中挂在
> `AgentEvent` 的 `turn_end`；两者经 `ExtensionUiRequest::Notify` 进聊天区（不落库、不进模型上下文）。
> TTL 复用 `cache_warmer::read_cache_retention` 的档位 + 目录 `promptCache.{short,long}`，与保温判据同源。
> 意外未命中用**保守信号**（本轮重新写入 + 上轮 TTL 内写过），仍属近似判定、可能误报
> （压缩 / 改工具集）；同一份缓存只提示一次，新写入重置告警记录。时间戳用墙钟毫秒
> （TTL 本就是墙钟概念），便于测试构造「超 TTL」场景。
> 验证：`cargo test --lib extensions::cache_watch`（11 个用例：纯函数判定矩阵、TTL 查表、
> 端到端提示与去重、会话切换复位）。声明 `fork_project()` 指向 jcode 0.94.0（来源版本取自本档），供 `/extension` 面板展示来源链接。
> 默认不启用、`modes() == [Dev, Creator]`。

### 3.10 浏览器自动化（`browser`）

| 维度 | 内容 |
|---|---|
| 一句话 | 给模型一个 `browser` 工具，**真正驱动浏览器**：导航、读页面快照、点/填/选、按键、滚动、截图、执行 JS、上传文件 |
| jcode 出处 | 工具 `crates/jcode-app-core/src/tool/browser.rs`(1272) + `browser_fast.rs`(763) + `browser_fast_actions.rs`(465) + `browser_jev.rs`(568)；桥接层 `crates/jcode-base/src/browser.rs`(1663+) / `browser_session.rs` / `browser_detect.rs`；协议 `docs/BROWSER_PROVIDER_PROTOCOL.md`(817)；handoff 说明 `docs/BROWSER_FAST_AGENT.md`(255) |
| jcode 语义（实测） | 动作集（`browser.rs:242-247`）：`list_tabs`/`new_tab`/`select_tab`/`get_active_tab`/`list_frames`/`open`/`snapshot`/`get_content`/`interactables`/`click`/`type`/`fill_form`/`select`/`wait`/`screenshot`/`eval`/`scroll`/`upload`/`press`，外加 `status`/`setup`/`handoff`。后端是**外部项目** `firefox-agent-bridge`（`browser.rs:8` 取 GitHub release）：装浏览器扩展 + native host + CLI，jcode 侧每个会话起一个 daemon 持有到 native host 的 WebSocket（端口按浏览器分配，`browser_session.rs:1-8`）。`handoff`（默认路径）把 goal + `tab_id` 交给一个由 Jev 类型化决策驱动的受限控制器做「观察 DOM → 选动作 → 执行 → 再观察」循环（`max_steps` 默认 12/上限 30、180s 墙钟、`confidence_threshold` 默认 0.8），只在需要「可执行脚本」或「精确文本」时 `hand_back` 回主模型；密码/OTP/支付凭据不得经 `text_values` 传递，页面内容视为不可信（`BROWSER_FAST_AGENT.md:53-91`）。`setup`/`status` 负责下载 bridge、校验版本与动作能力（能力探针见 `browser.rs:35-45`） |
| prux 现状 | 只有 `web_access` 的 `web_search` / `fetch_content`，是**读正文**（readable→markdown），不是浏览器控制 |
| prux 落点 | 扩展 `browser`：`tools()` 提供 `browser` 工具。后端二选一：① **CDP**（Chrome/Chromium 的 `--remote-debugging-port`，协议公开稳定，prux 已有 `tokio-tungstenite 0.30`，`Cargo.toml:160`）——首版推荐；② 复用 `firefox-agent-bridge`（要装外部 bridge，jcode 走的就是这条）。动作集先做直接动作（导航/快照/点击/输入/截图/eval），`handoff` 那种「整个任务交给控制器」可用 `subagent` 扩展的子代理循环表达，不必移植 Jev。截图经 `utils/terminal_image.rs`（已有 kitty/iterm2/sixel/halfblocks）显示 |
| 接缝与约束 | 工具执行是正常 I/O，可 `spawn` 连接与命令；`status`/`overlay_view` 里**不能**连浏览器（每帧调用、必须无 I/O），连接状态缓存进扩展 state。连接生命周期跟会话绑定（`on_session_switched` / `on_enabled_changed` 清理）；`ws://` 只允许 loopback，判定复用 `utils/net.rs:63` 的 `is_loopback_host` |
| 风险 | 入口前置条件重（用户要先起带调试端口的浏览器，或装 bridge）——文档必须写明「没有浏览器就不可用」；页面内容不可信（提示注入），要照 jcode 标注并限制敏感操作；截图/输出要有字节上限；CDP 端点无鉴权，绝不能监听非 loopback |
| 验证 | 本地起一个假 CDP WebSocket 服务回放握手与命令，断言各动作的参数整形、结果解析、超时与错误分类；用 `run_with_timeout` 包住，全程不联网；终端图片渲染走既有纯函数 |
| 工作量 | 大（CDP 客户端 + 动作集 + 截图显示；不含 handoff/Jev） |
| 默认状态 | 默认不启用（`Dev` / `Creator`） |

> **落地记录（2026-10-11）**：按本节的推荐口径实现，落点 `src/extensions/browser/`
> （`browser.rs` 扩展层 + `cdp.rs` 传输层 + `actions.rs` 动作层 + `snapshot.rs` 页面快照 + `config.rs` 配置）。
> 后端取 **CDP 直连**（`GET /json/version` 发现 + 浏览器级 WebSocket + `Target.attachToTarget{flatten:true}`
> 的 flat session 多标签页），不引入 `firefox-agent-bridge`；动作集为推荐的 16 个直接动作
> （`status`/`list_tabs`/`new_tab`/`select_tab`/`open`/`snapshot`/`get_content`/`click`/`type`/`select`/
> `press`/`scroll`/`wait`/`eval`/`screenshot`/`upload`），**不含** `handoff`/`setup`/Jev；
> 截图落盘到 OS 临时目录（`0o600`）并作为图片附件回给模型（附件由 TUI 既有的 `terminal_image` 渲染，
> 扩展不需要自己画图）。连接惰性建立、会话切换/禁用时丢弃；
> 只允许回环端点（复用 `utils::net::is_loopback_host` 的语义并归一 IPv6 方括号写法）。
> 验证：`cargo test --lib extensions::browser`（51 个用例：纯函数单测 + 假 CDP WebSocket/HTTP
> 回放服务驱动的动作集成测，全程不联网、只监听 `127.0.0.1` 随机端口）。
> 另用本机 Brave（Chromium 152，`--headless=new` + 临时 `--user-data-dir`）跑过一次真实端到端验证：
> 导航→快照→点 `ref`（页面 JS 真的跑了，标题被改）→输入→下拉→按键→滚动→等待→整页截图→
> 读正文/HTML→新标签页与切换，全部通过（临时测试已删，不进仓）。
> 默认不启用、`modes() == [Dev, Creator]`；配置在 `agent_dir()/extensions/browser.json`。
>
> **落地记录（2026-10-17，自动启动）**：新增 `src/extensions/browser/launch.rs`
> （端口探活 / 空闲端口 / 进程扫描 / 分离启动 / 等端点就绪），配置新增 `browsers` 候选列表。
> `ensure_connection` 改为：配置端口已在监听 → 试着连；连不上（被别的服务占着 / 不是 CDP 端点）也
> 不当作致命错误，照样往下走候选。候选分**两轮**处理：第一轮按 `browsers` 顺序连「已在跑且开着调试
> 端口」的第一个（可能是配置之外的端口），第二轮才挑第一个「没在跑」的候选用空闲端口启动并等端点
> 就绪（`timeout` 兼作预算）；「已在跑但没带 `--remote-debugging-port`」的候选一直跳过（再启动只会
> 转交给已有实例）。两轮必须分开：一轮循环会把「第一个候选没在跑」直接变成启动，把后面那个现成
> 端点盖掉（回归用例 `a_running_candidate_with_a_debug_port_wins_over_starting_one` 盯住这点）。
> 进程扫描用 `sysinfo`（`default-features = false, features = ["system"]`，不拉磁盘 / 网络 / CPU 采集），
> 一次拿到每个进程的可执行文件路径与命令行 argv，不再自己写 `/proc` / `ps` / PowerShell 三套分支。
> 命中判定看进程的 exe 与 argv0 两个文件名**任一**是否等于候选别名：只看 exe 会在 NixOS 那种
> coreutils 多合一二进制上失手（`sleep` 的 exe 是 `coreutils`），只看 argv0 会在没 `exec` 的 shell
> 包装脚本上失手；而按「任一参数」匹配会把 `grep browser` 误判成浏览器在跑。
> **默认候选不含 Firefox**（明确决策）：实测本机 Firefox 155 的 `--remote-debugging-port` 端点没有
> `/json/version`、`/json/list`（只剩 WebDriver BiDi 的 `/session`），显式写 `remote.active-protocols = 1`
> 也一样——该 pref 已随 CDP 后端在 141 一起删除；Firefox 140 ESR 默认同样 404，profile 里写
> `user_pref("remote.active-protocols", 3)` 后 `/json/version` 才回来（实测返回 `webSocketDebuggerUrl`）。
> 所以它不进默认列表，但保留两条可用性：写进 `browsers` 会按家族拿到自己的开关
> （`--no-remote --new-instance`），等不到端点时 `cdp_hint()` 把原因写进报错。
> 另外实测 Firefox 不会因 Chromium 的那两个开关（`--no-first-run` / `--no-default-browser-check`）起不来，
> 自定义项判错家族也不会崩。
> 验证：`cargo test --lib extensions::browser`（73 个用例）+ 本机 Brave 152 真实端到端一次
> （临时 wrapper 脚本加 `--headless=new` 与临时 `--user-data-dir` 走 `browsers` 配置，
> 启动 → 发现端口 → `open file://…` → `snapshot` 全部通过；另单跑一次真进程扫描，
> `running_state("brave")` 正确报出 `DebugPorts([19995])`；临时测试已删，不进仓）。

---

## 四、明确不实现（**均不做**）

### 4.1 破坏性命令门禁（`command-risk`）

| 维度 | 内容 |
|---|---|
| jcode 出处 | `crates/jcode-command-risk/`（`lib.rs` 573 + `gate.rs` 126 + `paths.rs` 321 + `tokenize.rs` 484）；动机 issue #604（`lib.rs:1-25`） |
| jcode 语义 | 四级 `RiskLevel{Safe,Low,Confirm,Catastrophic}`（`lib.rs:38-58`），`assess()` 纯确定性分类（`lib.rs:226`），`gate()` 两阶段（`gate.rs:76`）；**写死在 `bash` 工具实现里**（`tool/bash.rs:884` → `bash_destructive_gate.rs:14`），**无配置、始终启用** |
| 决定 | 🚫 **不作为扩展**。它是 core 的**安全策略**，不是可选功能：安全门禁必须无条件生效，一个要用户手动开启的扩展门禁等于没有。prux 若要它，落点是 `src/core/tools/bash.rs`（core 工程），不是 `src/extensions/` |
| 翻案条件 | 若将来做进 core，再单独立项；扩展层不再讨论 |

### 4.2 用户可配置生命周期 hooks（`hooks`）

| 维度 | 内容 |
|---|---|
| jcode 出处 | `docs/HOOKS.md`；`crates/jcode-base/src/hooks.rs`（1131 行）；配置 `[hooks]` 在 `crates/jcode-config-types/src/lib.rs:901`，事件枚举 `:918-958` |
| jcode 语义 | 8 个事件（`turn_start`/`turn_end`/`session_start`/`session_end`/`pre_tool`/`post_tool`/`post_tool_feedback`/`pre_tool_transform`）；`pre_tool` 是同步门禁（exit 2 阻止），其余是观察者 |
| 决定 | 🚫 **不作为扩展**。它是**扩展机制本身的一环**（用户脚本层的钩子总线），不是一项独立功能；prux 的钩子总线在 `src/core/extensions/hooks.rs`，要加用户脚本也应加在那层，而不是另起一个与之并列的扩展。已在 `extensions` 里的 `notify`（单事件 shell 钩子）是**功能**，它不构成「hooks 也是扩展」的先例 |
| 翻案条件 | 若将来要做，属 core 扩展机制的增强，单独立项 |

### 4.3 任务质量门 / 催办（`task-policy`）

| 维度 | 内容 |
|---|---|
| jcode 出处 | `crates/jcode-todo-policy/`（`lib.rs` 895 + `followup.rs` 394 + `store.rs` 191） |
| jcode 语义 | `FollowUpPolicy::decide`（`followup.rs:168`）产出 7 种 `FollowUpKind`（`followup.rs:68`），熔断 `MAX_GATE_ATTEMPTS=5`（`:27`），合成消息持久化但 UI 隐藏（`lib.rs:718,781`） |
| prux 现状 | `extensions/tasks/` 已有提醒 cadence（`tasks/cadence.rs`、`tasks.rs:641-652`），只有「催用任务工具」一档 |
| 决定 | 🚫 **不作为独立扩展**。它是 `tasks` 扩展的**辅助项**（策略层），应并进 `tasks`；按本档尺子，辅助项不新起扩展。本轮**明确不做**（既不做成新扩展，也不并进 `tasks`） |
| 翻案条件 | 若 `tasks` 扩展将来要增强催办力度，作为 `tasks` 的一次改动处理 |

### 4.4 邮件通知渠道（`notify-email`）

| 维度 | 内容 |
|---|---|
| jcode 出处 | `crates/jcode-notify-email/src/lib.rs`（529 行：`send_email`:23 / `poll_imap_once`:67 / `parse_permission_reply`:158） |
| 决定 | 🚫 **不作为独立扩展**。它是 `notify` 扩展的**渠道**（`notify` 现在跑一条 settled 命令）。按本档尺子，渠道不新起扩展。本轮明确不做 |
| 翻案条件 | 若 `notify` 要加邮件渠道，作为 `notify` 的一次改动处理 |

### 4.5 子代理文件冲突感知

| 维度 | 内容 |
|---|---|
| jcode 出处 | `crates/jcode-app-core/src/server/file_touch_service.rs`（147 行）+ `file_activity.rs:39` + `server.rs:2184,2237` 发 `NotificationType::FileConflict` |
| jcode 语义 | 维护 path→accesses 与 session→paths 双向索引；分类 overlapping lines / 同文件不重叠；向双方注入 soft-interrupt |
| prux 现状 | `extensions/subagent/` 的子代理之间**无**文件冲突通知 |
| 决定 | 🚫 **不作为独立扩展**。它是 `subagent` 扩展的**辅助项**，应并进 `subagent`；且 jcode 的实现是 **server 级索引**（要跨 session，见 §4.6），prux 进程内只能做弱化版。本轮明确不做 |
| 翻案条件 | 若 `subagent` 要加冲突提醒，作为 `subagent` 的一次改动处理 |

### 4.6 单 server 多 client 与 swarm

jcode 的 swarm 建立在**单 server 持有全部 session** 之上：`docs/SERVER_ARCHITECTURE.md`（Unix
socket，多 client）、`docs/SWARM_ARCHITECTURE.md`（332 行）、`docs/SWARM_TASK_GRAPH.md`（605 行）、
`crates/jcode-swarm-core/`（874 行）、`server/swarm.rs`（3440 行）、`tool/communicate.rs`（3490 行，
`swarm` 工具动作见 `:2012-2017`）、headless worker（`server/headless.rs:37`）、
文件冲突通知（`server/file_touch_service.rs`）、agent 间 DM/broadcast（`server/client_comm_message.rs:103`）、
以及按 `swarm_id` 共享的任务 DAG（`crates/jcode-plan/`）。

**为什么不做成扩展**：swarm 的每一条能力（跨 session 的 spawn 树、文件冲突索引、消息路由、
共享 plan）都以「服务端进程持有多个 session」为前提。prux 是**单进程 TUI**，扩展活在当前
会话内，没有服务端、也没有第二个 session 可协调。要做就得先做服务端——那是新的运行模式
（与 kiss 档 §3.1 的 ACP 同类），属 core 工程。

### 4.7 ambient 守护进程与 overnight

jcode 的 ambient 是**单实例后台守护循环**：一次完成 garden（记忆整合）+ scout（会话/git/记忆）
+ work（主动任务），有自适应间隔与崩溃恢复（`docs/AMBIENT_MODE.md` 939 行；
`crates/jcode-app-core/src/ambient/{runner.rs 1110,scheduler.rs 497,manager.rs 110}`）。
`/overnight <hours> [mission]` 是带检查点的长时 mission（`crates/jcode-overnight-core/` 1471 行 +
`app-core/src/overnight.rs` 1275 行）。

**为什么不做**：守护进程要**独立生命周期与单实例锁**，扩展无法接管进程；ambient 还依赖
§3.1 的记忆系统与 §4.6 的服务端。overnight 可由 `goal` + `subagent::schedule` 拼近似，
但 jcode 的 checkpoint/report 阶段机（`prompts.rs:7`）不是扩展能完整表达的。

### 4.8 对外接口（harness-api / protocol / SDK / gateway / remote / SSH）

jcode 有版本化 NDJSON 的 `jcode-harness-api`（`crates/jcode-harness-api/src/lib.rs`，含握手与
未知帧容错）、内部 `jcode-protocol`、`jcode-sdk`、gateway，以及 remote compile / native SSH
（`docs/REMOTE_COMPILE.md`、`docs/NATIVE_SSH.md`）。prux 无任何服务端形态。
**为什么不做**：与 kiss 档 §3.1 同因——都是**新的运行模式**，要在 `cli/` 选模式、
在 `modes/` 加入口、把事件序列化出去；扩展只在交互会话内活动。属 core 工程。

### 4.9 self-dev 自修改

jcode 的 self-dev 让 agent 改自己的源码、编译、测试、热重载二进制（`tool/selfdev.rs` +
`register_selfdev_tools`，`tool/mod.rs:1721`；`debug_socket` 在 `:1730`；`jcode-selfdev-types`）。
**为什么不做**：需要接管构建与进程重启，扩展层够不到。

### 4.10 跨 harness 会话恢复（沿用 kiss §3.9）

jcode 的 `/resume` 能接着用 codex / claude code / opencode / pi 的会话
（`crates/jcode-import-core/` 2421 行 + `session_search` 的外部加载）。prux 侧
`migration-kiss-v0.0.35.md` §3.9 已判**不做**：语义对不齐、长期维护、一次性收益、不在 pi 主线。
jcode 的实现证明「技术上可行」，但**不改变结论**——它同样要永久跟别家私有 jsonl 走。
（**只读检索**别家会话同理，故 §3.2 也只搜自家。）

### 4.11 多账号切换（沿用 kiss §3.7）

jcode 支持同 provider 多账号快速切换（`/account`）。prux 的 `auth.json` 形状是 **pi 契约**
（`packages/ai/src/auth/types.ts`「One type-tagged credential per provider」），
改成池 = 主动偏离 pi + 迁移老用户文件；且换账号要重发同一请求，`provider_stream_event`
拦不住，属 core 改动。kiss 档 §3.7 已判不做，**结论不变**。

### 4.12 新 provider 原生适配

jcode 为 Cursor / Bedrock / Antigravity / Copilot / OpenRouter 等各做了 native runtime
（`crates/jcode-provider-*-runtime/`）。prux 的 provider 目录由 `scripts/sync-models.py`
从 pi 同步，`SUPPORTED_PROVIDERS`（`src/core/model_resolver.rs:41`）是唯一真源。
**为什么不做**：provider 适配属 `core::provider`。OpenAI 兼容或近兼容的可用已有的
`proxy` 扩展桥接。

### 4.13 桌面 applet / iOS / 远程客户端

jcode 有 `applet` 工具（桌面原生 UI 卡片）、`desktop_selfdev`、iOS app、多 surface 客户端
（`docs/MULTI_SESSION_CLIENT_ARCHITECTURE.md`）。**为什么不做**：都不是终端 TUI 能力，
且依赖 §4.6 的服务端与桌面运行时。

### 4.14 Composio 式集成（gmail / calendar / integration_tools）

jcode 有 `gmail` / `calendar` / `integration_tools`（默认开）与 `docs/GMAIL_COMPOSIO_BACKEND.md`。
**为什么不做**：第三方 SaaS 集成需要 OAuth 应用注册与长期维护，且不属「coding agent」主线；
真要用可由用户经 MCP 挂上（prux 已有 `mcp` 扩展）。

### 4.15 本地 embedding 栈

jcode 本地嵌入用 `all-MiniLM-L6-v2`（ONNX/tract，`crates/jcode-embedding/src/lib.rs`），
**默认不编入**（opt-in feature）。prux 无 embedding 依赖，也没有 embedding 型 provider API
（`Extension` 只有 `image_apis()` / `classifier_apis()`）。**为什么不做**：本地 ONNX 引入
native 依赖（打包与跨平台成本高，同 kiss 档 §3.3 拒语音的理由）；云端 embedding 要外发
上下文且需要一个新 API 面。**决定**：§3.1 的记忆扩展走**词法**召回（jcode 自己的自动召回
也已不依赖余弦），embedding 不做。

### 4.16 provider-doctor 的 catalog / full 档

jcode 的 `provider-doctor` 有三档：`Offline`（无 key 无网）、`Catalog`（需 key、近零花费，
拉 `/models`）、`Full`（需 key、**有花费**，跑非流式 + 流式 + 工具调用回路，12 个检查点；
`crates/jcode-provider-doctor/src/provider_e2e.rs:44`、`docs/PROVIDER_DOCTOR.md:38-67`）。
prux 的 `doctor` 扩展只做不带凭据的 `HEAD` 可达性探测。
**为什么不做**：`Full` 会**真实计费**并需要凭据，与 prux `doctor`「不发 prompt、不动凭据、
只证明网通」的设计目标冲突；`Catalog` 档要引入凭据使用，属对既有 `doctor` 扩展的增强
（辅助项），按本档尺子不做。

---

## 五、落地后的收尾检查（**通用清单**）

> 本节是**通用清单**，不是逐项记录：每落地一项，按下列各条自查，
> 再把验证命令与结果写进该节末尾的「落地记录」块。

- [ ] **渲染与 I/O 的边界**：新增扩展的 `overlay_view` / `dock_lines` 只画缓存数据；
      读盘 / 聚合 / 网络 / 外部命令分别在命令执行时或 `spawn` / `spawn_blocking` 里完成。
- [ ] **changelog**：按 `Cargo.toml` 当前 `version`（现为 `1.1.0`）更新
      `assets/changelog/v.<version>.md`（本仓规定缺文件会编译失败）。
- [ ] **扩展文档**：`assets/docs/extensions.md` 增表格行 + 小节，并登记进「默认关闭」清单。
- [ ] **扩展注册表测试**：`tests/extensions_registry.rs` 的计数与注册序断言随扩展数更新，
      并逐个断言新扩展默认不启用。
- [ ] **默认状态**：新扩展一律 `default_enabled() == false` + `modes() == [Dev, Creator]`。
- [ ] **来源声明**：从 jcode 移植的扩展一律实现 `Extension::fork_project()`，返回
      `jcode` / 本档冻结的版本号（现为 `0.94.0`）/ `https://github.com/1jehuang/jcode`——
      `/extension` 面板靠它展示来源链接，漏写不会编译报错，只会静默少一行。落地记录里同步写明。
- [ ] **单测**：每个扩展的核心决策写成纯函数单测；网络/外部进程测试用 `run_with_timeout` 包住；
      目录隔离用 `AgentDirGuard::temp()`（异步测试用 `pin_test_agent_dir`）。
- [ ] **`cargo fmt` / `cargo clippy --all-targets` / `cargo test`** 全绿后再提交；
      格式化改动与逻辑改动分开提交。

---

## 六、复核记录与待确认项

### 6.1 已核对的事实与易错点

| 项 | 易错点 | 实测 |
|---|---|---|
| jcode 版本 | 文件名 `v0.94.0` 是否等于检出版本 | 一致：`Cargo.toml:3` 为 `0.94.0`，检出 `2a9e7f624` |
| 记忆系统架构 | README 说「每轮嵌入 + 余弦召回」 | README.md:311-320 已过期；`docs/MEMORY_ARCHITECTURE.md` 与代码是 **Jev 类型化决策召回**，余弦只在遗留 hybrid 路径 |
| 本地嵌入是否默认开 | — | **否**，`embeddings` 是 opt-in feature（`Cargo.toml:210-227`），stub 直接报错 |
| swarm 是否可用扩展表达 | — | 否：依赖单 server 多 session（§4.6） |
| `command-risk` 的归属 | 是不是「功能」 | **不是**：是 core 安全策略，写死在 `bash` 工具里（`tool/bash.rs:884`） |
| `hooks` 的归属 | 是不是「独立功能」 | **不是**：是扩展机制的用户脚本一环，与 prux 的钩子总线同层 |
| `provider-doctor` 的 full 档是否计费 | — | **是**，`DoctorTier::spends_balance`（`provider_e2e.rs`） |
| agentgrep 是否自研 | — | 底层是**外部 crate** `agentgrep v0.1.7`（`crates/jcode-app-core/Cargo.toml:111`），jcode 只封装 |
| `bg` / `panel` / `open` 是否被别的东西覆盖 | — | 未被 prux 任何现有扩展覆盖（`usage` 落地后仍成立：它不做本地台账，与 `cache-usage` 不重叠） |
| `browser` 是否算「独立扩展」 | 需外部 bridge 是否等于「够不到扩展层」 | **不算够不到**：外部依赖只是运行期前置条件（同 `mcp` 需 MCP server、`web-access` 需网络）；扩展可自建 CDP/bridge 客户端，故列为 §3.10 |

### 6.2 落地前需要确认的决策点

1. ~~**§3.1 `memory` 的召回范围**~~：已定——只做**词法**（BM25），不补 embedding API 面（2026-10-19，见 §3.1 落地记录）。
2. ~~**§3.3 `agent-grep` 的结构提取**~~：已定——**不做**（2026-10-19，见 §3.3 决定）。
3. ~~**§3.5 `panel` 的终端降级形态**~~：已定——**不做**（2026-10-19，见 §3.5 决定）。
4. ~~**§3.4 `usage` 的数据源**~~：已定——只做**配额端点**，不做本地台账（2026-10-19，见 §3.4 落地记录）。
5. ~~**§3.6 `pdf` 的落点**~~：已定——走「新起 `read_pdf` 工具」，不动内置 `read`（见 §3.6 落地记录）。
