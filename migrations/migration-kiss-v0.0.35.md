# Prux 迁移指南：kiss → prux 扩展（候选清单 + autoresearch 落地）

> **来源**：`kiss`（`https://github.com/racetozero/kiss`，Rust 写的终端 coding agent，
> 同样基于 pi；本仓的本地只读检出是 `kiss -> /home/blue/wayshot-fork/kiss`，
> 检出点 `v0.0.35-5-gdd29d0d`：`[workspace.package] version = "0.0.35"`，
> `[workspace.metadata.pi] release = "v0.99.1"`）。
>
> **版本修订（第二版）**：首版写于 kiss 检出还在 `v0.0.18` 时（文件名里的 `0.0.35` 是当时
> 下发任务的编号，与检出版本并不一致）；本版按 `v0.0.35` 逐行重核，修订与新增见 §6。
> 最大的修订是：首版 §4 记的 autoresearch「实施记录」当时**并不存在**（仓里没有那些文件、
> git 历史里也没有），本版 §4 换成本次真正落地的记录。
>
> **版本修订（第三版）**：把 §2.2 凭证导入、§2.5 跨 agent 会话恢复、§2.8 自更新、
> §2.9 多账号池、§6.2 上下文文件五项**明确判为不实现**（理由与翻案条件集中在 §3.5–§3.9），
> 并把剩下的待做项按 kiss 实测行为补齐落点、接缝与验证方式。
>
> **版本修订（第四版）**：§2.3 / §2.4 / §2.7 三项**已按第三版的设计落地为四个内置扩展**
> （`cache-usage` / `doctor` / `btw` / `recap`，实施记录见 §4.5–§4.7）；四个都是
> **默认不启用**、只在 `Dev` / `Creator` 扩展模式下可用。§2.7 按"旁问只在用户开口时才有动作、
> 小结会在后台自己花 token"拆成了两个独立开关的扩展。§2.6 WebMCP 是唯一还没落地的候选功能。
>
> **目标**：`src/extensions/` 下的扩展（不与上游插件移植混在一起；本仓已有
> `migration-subagents.md` / `migration-tasks.md`）。
> **性质**：先冻结「值得移植 / 不值得移植」的判断，再逐条落地；落地项的状态列在实施过程中回填。
>
> **与既有迁移文档的关系**：kiss 与 prux 是**同源重写**（都对齐 `@earendil-works/pi`），
> 所以本文档记的不是「某个上游插件的映射」，而是「同一个 pi 契约在两套 Rust 实现之间的
> 功能差集」。凡是 pi 契约本身的差异，都以 prux 侧的 pi 对齐文档为准（`migration-v*.md`）。

---

## 〇、摘要

### 结论

kiss 与 prux 的功能重叠度很高，**自动化那一半基本已经对齐**：子代理、动态工作流、定时作业、
QuickJS 沙箱、MCP、联网检索、技能与提示词模板、项目信任、缓存保温、本地 OpenAI 兼容网关，
在 prux 里都有对应实现（对照见 §1.2）。

按 `v0.0.35` 重核、逐项决策并落地后，清单收口为：

- **已落地（4 项 → 5 个扩展）**：autoresearch 闭环（§2.1，`subagent` 扩展，记录见 §4.1–§4.4）、
  缓存效率跨会话报告（§2.3 → `cache-usage`，§4.5）、
  provider 可达性诊断（§2.4 → `doctor`，§4.6）、
  只读旁问与一行小结（§2.7 → `btw` + `recap` 两个扩展，§4.7）。
  新增的四个扩展一律**默认不启用**（`Dev` / `Creator` 模式），理由见各小节；
- **待做（1 项）**：WebMCP（§2.6）——唯一还没落地的功能项（实验性 Chrome API，价值低）；
- **明确不实现（5 项）**：外部 agent 凭证导入（§3.5）、真实自更新（§3.6）、
  多账号池与限流切换（§3.7）、实验性上下文文件（§3.8）、跨 agent 会话恢复（§3.9）。

另有四类**不该做成扩展**（§3.1–§3.4）：对外接口（ACP / RPC / SDK）、provider 层新增（Cursor /
Databricks / Snowflake / Bedrock）、语音输入、Jev 压缩。前两类需要进程级接线与 `core::provider`
改动，扩展层够不到；后两类成本与收益不匹配。

### 路线图

| Tier | 项 | 状态 |
|---|---|---|
| 1 | §2.1 autoresearch 闭环 | ✅ 已落地（§4.1–§4.4） |
| 2 | §2.3 缓存效率跨会话报告 | ✅ 已落地（§4.5，`cache-usage`） |
| 3 | §2.4 provider 可达性诊断 | ✅ 已落地（§4.6，`doctor`） |
| 4 | §2.7 `/recap` + `/btw` | ✅ 已落地（§4.7，`btw` + `recap` 两个扩展） |
| 4 | §2.6 WebMCP | 🔜 待做（唯一未落地项） |
| — | §2.2 凭证导入、§2.5 会话恢复、§2.8 自更新、§2.9 多账号池、§6.2 上下文文件 | 🚫 决定不实现（§3.5–§3.9） |
| — | §3.1–§3.4 不做成扩展的四项 | 🚫 已定案 |

---

## 一、架构对照

### 1.1 两仓的模块形状

| 维度 | kiss | prux |
|---|---|---|
| 顶层 | workspace（`crates/kiss-*` 十个 crate，语言绑定在 workspace 外） | 单 crate（`src/` 分层，见 `AGENTS.md`） |
| 扩展体系 | **无**：功能直接写在 crate 里（`kiss-coding` 是"业务功能层"） | `src/extensions/` + `linkme` 静态注册 + `Extension` trait |
| 命令注册 | 集中式 `crates/kiss/src/slash_commands.rs`（`v0.0.35`：Pi 核心 23 + KISS 本地 17；首版写的 "24 + 17" 是两个版本的混搭） | 每个扩展 `commands()` 声明，`on_registered` 里 `register_slash_command` 接线 |
| 作业/后台 | `kiss-coding/src/iterative.rs`（loop/autoresearch 作业）+ `kiss/src/job_ui.rs`（进度视图） | `extensions/subagent/`：`manager`（子代理注册表）+ `workflow/task`（工作流运行）+ `schedule`（定时）+ `autoresearch`（目标迭代） |
| 对外接口 | JSONL RPC / WebSocket RPC / ACP / 六种语言绑定 | 无（仅 MCP 客户端 + 扩展间事件总线） |

### 1.2 prux 已覆盖（**不需要移植**）

逐项对照 kiss 的能力与 prux 的对应实现：

| kiss | kiss 落点 | prux 对应 | 结论 |
|---|---|---|---|
| 子代理（6 工具：spawn/send/followup/wait/list/interrupt） | `kiss-coding/src/subagents.rs`（917 行） | `extensions/subagent/`（`Agent` / `get_subagent_result` / `steer_subagent` + 类型发现 + 并发池 + resume） | ✅ 已有 |
| 动态工作流（模型写 JS 编排，最多 1000 子代理） | `kiss-coding/src/workflows/`（约 1300 行） | `extensions/subagent/workflow/`（`SubagentWorkflow` 工具 + rquickjs 沙箱 + `pipeline`/`parallel`/`gate`/`resume`/journal/inspector） | ✅ 已有（prux 侧更全） |
| 定时/循环作业 | `iterative.rs` + `kiss/src/job_ui.rs` | `extensions/subagent/schedule.rs`（6 段 cron/间隔/一次性 + `/agents schedules`）+ `Agent` 工具的 `schedule` 参数 | ✅ 已有 |
| 「一次脚本编排」的作业视图 | `kiss/src/workflow_ui.rs` | `extensions/subagent/workflow/{progress,inspector,task}.rs` | ✅ 已有 |
| MCP（本地/远程/OAuth） | `crates/kiss-mcp/` | `extensions/mcp/` | ✅ 已有 |
| 联网检索与网页回读 | （kiss 内置 web 工具） | `extensions/web_access/`（`web_search` / `fetch_content` / `get_search_content`） | ✅ 已有 |
| 脚本沙箱执行 | 无（kiss 用工作流脚本） | `extensions/codemode/`（QuickJS + `ALL_TOOLS`） | ✅ 已有 |
| 技能 / 提示词模板 / 上下文文件 / 项目信任 | `kiss-coding/src/{skills,prompts,context_files,trust}.rs` | `core/{skills,prompt_templates,context}.rs`、`core/project_trust.rs`、`extensions/skills.rs` | ✅ 已有 |
| 缓存保温（`cacheWarming` off/idle） | kiss 设置项 | `core/cache_warmer.rs` + `Extension::cache_warming_decision` | ✅ 已有 |
| 自定义 OpenAI 兼容 provider | `kiss/src/provider_cli.rs`（`kiss provider add`） | `extensions/proxy/`（本地兼容网关）+ `models.json` | ✅ 等价（机制不同） |
| 循环/假死检测 | 无（kiss 靠用户） | `extensions/loop_detect/`、`extensions/hang_detect/` | ✅ prux 独有 |
| 会话导出 HTML / 分支 / 克隆 / 树 | `kiss/src/export.rs` 等 | `core/export_html*`、`/fork` `/clone` `/tree` `/import` | ✅ 已有 |

### 1.3 扩展系统的能力边界（判断可行性的依据）

判「能不能做成扩展」看 `core::extensions::Extension`（`src/core/extensions.rs`）：

- **可注册**：工具（`tools()`）、斜杠命令（`commands()`）、CLI flag（`cli_flags()`）、快捷键
  （`keybindings()`）、设置项（`settings()`）、虚拟模型（`virtual_models()`）、MCP 声明
  （`mcp_servers()`）、工具渲染器（`tool_renderers()`）。
- **可画 UI**：`dock_lines()`（状态栏上方的 dock）、`overlay_view()` / `on_overlay_event()`
  （自定义覆盖面板）、`render_custom_message()`、`request_ui()`。
- **可挂钩子**：`transform_context` / `transform_context_with_system`、`before_tool_call(_ext)` /
  `after_tool_call(_ext)`、`on_agent_event`、`provider_stream_event`、`on_boundary`、
  `cache_warming_decision`、`on_user_prompt(_with_context)`、`on_session_start` /
  `on_session_switched` / `on_exec_ctx`。
- **可持久化**：写会话内 `Entry::Custom`（`goal` 扩展的做法）或自写 `agent_dir()` 下的文件
  （`hang_detect` / `subagent::schedule` 的做法）。
- **可联网**：扩展就是进程内 Rust，自建 HTTP 客户端没有限制（`web_access` / `proxy` / `downloader` 为证）。
- **不能**：改内置工具实现（扩展是第二层）；渲染路径必须同步、无 I/O；`provider_stream_event` /
  `on_boundary` 高频且禁止持 agent 锁；没有沙箱（扩展即进程内代码）。

**关键推论**：需要新的**运行模式**（如 ACP 服务端）或新的 **provider 适配**（如 Cursor 的原生
HTTP/2）时，扩展层够不到——那要动 `core/` 与 `cli/`。其余都能落在扩展里。

---

## 二、逐项总表

### 2.1 autoresearch 闭环（Tier 1，✅ 已落地）

| 维度 | 内容 |
|---|---|
| kiss 出处 | `crates/kiss-coding/src/iterative.rs`（601 行，`v0.0.18`→`v0.0.35` **未改**）：`JobKind::Autoresearch` + `iteration_prompt` + `contains_completion_marker`；视图在 `crates/kiss/src/job_ui.rs`（258 行，同样未改） |
| kiss 语义 | 从当前会话分叉持久子会话 → 每轮同一提示词（首轮建基线+成功指标，之后每轮一个小想法、同一套验证、留改进退退化）→ 回答里出现 `[goal-complete]` 即收工；上限 `--iterations N`（clamp 100）；同时最多 4 个作业 |
| prux 缺口 | 有 `schedule`（墙钟触发）与 workflow（脚本驱动），但**没有**「同一目标反复迭代直到达标」的状态机 |
| prux 落点 | `src/extensions/subagent/autoresearch.rs`（新模块）+ `Subagent::commands()` 的 `/autoresearch`（✅ 本次落地，实施记录见 §4） |
| 为什么在 subagent 扩展内 | 驱动子代理的唯一基础设施（`fleet::cached_ctx` / `manager::dispatch` / 通知 / dock 组合）都在这里；另起一个扩展只会把这些再实现一遍 |
| 为什么不套 workflow 引擎 | workflow 运行有 **30 分钟墙钟兜底**（`bridge::MAX_WALL_MS`），而 autoresearch 是长作业——会被那个兜底掐掉 |

### 2.2 外部 agent 凭证导入（Tier 2，🚫 决定不实现，见 §3.5）

| 维度 | 内容 |
|---|---|
| kiss 出处 | `crates/kiss-ai/src/auth/external.rs`（`v0.0.35` 为 775 行，较 `v0.0.18` +3）：发现并导入 OpenAI Codex、Claude Code（含 macOS 钥匙串）、Pi、OpenCode、OpenClaw、Hermes 的登录态；`crates/kiss/src/auth_flow.rs` 的 `kiss auth import` |
| prux 现状 | 只有 `auth.json` + OAuth 登录链路（`--api-key > auth.json > OAuth > models.json > env`），**没有**从其它 agent 导入（写入 API `write_auth_key` / `write_oauth_credential` / `remove_auth` 都在 `src/core/auth.rs` 公开，技术上做得到） |
| 决定 | 🚫 不实现。理由与翻案条件见 **§3.5** |

### 2.3 缓存效率跨会话报告（Tier 2，✅ 已落地，见 §4.5）

| 维度 | 内容 |
|---|---|
| 一句话 | 把每个会话 / 每个 provider 的缓存命中率画成趋势，回答“这个 provider 值不值得开缓存保温”；kiss 的入口是 CLI `kiss cache-usage` 与 `/cache-usage` |
| kiss 出处 | `crates/kiss/src/cache.rs`（458 行）+ CLI `kiss cache-usage`（`args.rs:210-219`、`main.rs:116-124`）+ `/cache-usage`（`interactive.rs:7276-7306`） |
| kiss 语义（实测） | **数据源就是己方持久化的 session JSONL**（`SessionManager::entries()`，`cache.rs:120-165`）：样本取自 assistant 消息与 Compaction / BranchSummary 的 usage，去重键 `entry_id:timestamp:provider:input:read:write`（`cache.rs:172-180`）。输出三组：Total history（累积面图）→ Provider history（按 provider）→ 各会话的条形（`cache.rs:203-241`）。命中率 = `read / (input+read+write)`，行尾 `X.X% · read / total input tokens · N responses`（`cache.rs:245-259`）。图形常量 `GRAPH_WIDTH=32` / `GRAPH_HEIGHT=4`（面图纵轴 0/25/50/75/100%，`cache.rs:299-330`）、`rate_bar` 24 格 `█`/`░`（`cache.rs:261-274`）。`/cache-usage` 空参=当前会话、`all`=session_dir 全部、`[provider]`=按该 provider 过滤（过滤同时作用于总量与各会话行）；CLI 的 `--session` 支持前缀匹配与文件路径（`cache.rs:75-107`）。**没有**时间窗口参数，全量绘制 |
| prux 现状 | 只有当前会话：`extensions/footer.rs` 的 `latest_cache_hit_rate`（`footer.rs:67-80`，公式与 kiss 相同）+ 累计 `cache_read` / `cost`；`core/cache_warmer.rs` 只管“要不要保温” |
| prux 落点（已按此落地） | 扩展 `cache-usage`：扫会话目录（`default_session_dir`，即 `handlers/session_repair.rs:238` 用的那个）下的 `*.jsonl`，逐份交给 `Session::open_checked`（`src/core/session_manager.rs:715`）解析，取 `Usage`（`src/core/provider.rs:979`：`input` / `output` / `cache_read` / `cache_write` / `total_tokens: u32`，以及 `cost: Cost`——`Cost` 在 `:963`，`input/output/cache_read/cache_write/total: f64` 美元）→ 命中率算法直接复用 `footer.rs` 那套 → `/cache-usage [all] [provider]` 命令 + `overlay_view` 画图。**不要自建 usage 文件**：首版写的“追加写 `agent_dir()/extensions/cache-usage.jsonl`”是多余的第二份真源，prux 的会话 jsonl 本身就是 kiss 那份数据源 |
| 接缝与约束 | `overlay_view`（`src/core/extensions.rs:757`）**每帧调用且必须同步无 I/O** → 读盘与聚合要在命令执行时算完、缓存在扩展 state 里，面板只做绘制。CLI 形态受 `cli_flags()` 限制：`CliFlagDef`（`extensions.rs:147`）只有 `name` / `description` / `takes_value`，而 `src/cli/args.rs:17` 的 `Command` 只有 `Auth` / `Mcp`——扩展**只能加 flag、加不了子命令**，所以 kiss 的 `cache-usage` 子命令在 prux 退化成 `prux --cache-usage [--cache-provider X]` 这类 flag |
| 验证 | 单测造 2–3 份临时会话 jsonl → 断言去重、聚合维度、命中率与条形文本；渲染照 `footer` 的做法用纯函数断言；目录隔离用 `AgentDirGuard::temp()` |
| 工作量 | 小（数据已在会话文件里，不需要新增采集） |

### 2.4 provider 可达性诊断（Tier 3，✅ 已落地，见 §4.6）

| 维度 | 内容 |
|---|---|
| 一句话 | 一条命令体检**所有** provider 的网络可达性：哪家连不上、断在哪一层（DNS / TCP / TLS / WebSocket / SSE）、要把哪个主机放行 |
| 谁用、什么时候用 | 公司代理 / 防火墙 / 严苛网络下的用户；以及“对话报错时，到底是 provider 挂了还是我这边的网挂了”——prux 目前**没有任何**能回答这个问题的东西（`core/bug_report.rs` 只收集崩溃与失败轮次的诊断信息，不测网） |
| 用完能得到什么 | ① 每家 provider 的 `STATUS/PROVIDER/CONNECTION/HOST:PORT/RESULT` 明细；② `--summary` 的 `REACHABLE/SKIPPED/FAILED` 计数（拿来决定“继续排障”还是“找网络团队”）；③ Environment 段：安装形态、目录数据、登录态、代理设置；④ 退出码 0/1，可直接进脚本 |
| kiss 出处 | `crates/kiss/src/doctor.rs`（732 行）；CLI 子命令 `kiss doctor [--summary]`（**不在** TUI 斜杠命令表里） |
| kiss 语义（实测） | 常量 `PROBE_TIMEOUT=10s`、`PROBE_CONCURRENCY=16`（`doctor.rs:8-9`）。探 registry 里**全部** provider、不按凭据过滤（`doctor.rs:180-208`）；**请求本身不带凭据**（避免计费与日志），只有推导 base_url 时会读本地凭据（`doctor.rs:210-260`）。协议判定：SSE = `HEAD`；WebSocket = `GET` 带 `connection` / `upgrade` / `sec-websocket-version` / `key`（key 硬编码），只对 codex / azure / openai-responses（`doctor.rs:314-334`、`368-392`）；`bedrock-converse-stream` = EventStream `HEAD`；Login = 各 OAuth 端点 `HEAD`（`doctor.rs:430-480`）。**200 与 401 都记 OK**——端点在响应就说明网通，鉴权对不对是另一回事（`doctor.rs:404-428`）。退出码：本地失败或有 failed → 1（`doctor.rs:143`）。kiss **没有**自动生成“防火墙白名单”（README:310-311 只是建议把失败行交给网络团队）——本节首版写的“出防火墙白名单表”以此为准 |
| prux 落点（已按此落地） | 扩展 `doctor`：**TUI 内的 `/doctor [summary]` 命令 + `overlay_view` 画表**。数据源：`assets/models/providers.json`（顶层字符串数组）拿 provider 名单，`find_model`（`src/core/model_resolver.rs:628`，给 `ModelEntry.api` / `ModelEntry.base_url`）或 `model_config::provider_config`（`src/core/model_config.rs:80`，给 `ProviderConfig.base_url` / `.api`）拿端点与协议；并发用 `Semaphore(16)`；HTTP 用已有的 `reqwest 0.13`（`Cargo.toml:128`），WebSocket 的 101 判定用 `tokio-tungstenite 0.30`（`Cargo.toml:160`） |
| 形态限制 | kiss 那种“CLI 子命令 + stdout 表 + 退出码”在扩展层**做不到**：扩展只能加 flag（`cli_flags()` → `CliFlagDef`），而 flag 的语义是“启动配置”（先例是 `--subagents-workflow-file`，`extensions/subagent.rs:655-668`），不是“跑完打印就退出”。要 CLI 形态就得进 `src/cli/` 的内置子命令（core 工程）；扩展版本先把结果画进覆盖面板，`/doctor summary` 切汇总视图 |
| 接缝与约束 | 探测必须**不发 prompt、不动凭据**（照 kiss）；`overlay_view`（`src/core/extensions.rs:757`）每帧调用且必须同步无 I/O，所以探测要在命令执行时做完、结果缓存进扩展 state。loopback 判定可复用 `utils/net.rs:63` 的 `is_loopback_host` |
| 风险 | 探测会打到真实 provider 端点（留日志、可能触发 WAF），10s 超时与 16 并发上限要照抄，不要“顺手调大”；结果会随时间过期，面板上要带时间戳 |
| 验证 | 不对着真网络测：把探测目标做成可注入，本地起测试 HTTP 服务分别返回 200 / 401 / 挂起，断言分类结果、汇总计数与超时行为；整段用 `run_with_timeout` 包住 |
| 工作量 | 中（探测本身简单，工作量在端点分类与输出整形） |

### 2.5 跨 agent 会话恢复（Tier 3，🚫 决定不实现，见 §3.9）

| 维度 | 内容 |
|---|---|
| 一句话 | kiss 的 `/resume` 选择器能直接列出并**接着用**别家 agent（Pi / Claude Code / Codex）的会话 |
| kiss 出处 | `crates/kiss/src/session_sources.rs`（434 行）：4 个源（`session_sources.rs:13-31`、`58-75`）——KISS（`kiss_dir`）、Pi（`~/.pi/agent/sessions`）、Claude Code（`~/.claude/projects`）、Codex（`~/.codex/sessions`）；`Ctrl+G` 在 picker 内把 `global` 取反重开（`interactive.rs:6074-6078`），非 global 时按 `cwd == 当前` 过滤（`session_sources.rs:80-82`） |
| prux 现状 | 有 `/resume`（自己的会话）与 `/import <path>`（自己的 jsonl：`handlers/commands.rs:669` → `run_import`（`handlers/session_repair.rs:238`）→ `Session::open_checked`（`session_manager.rs:715`）），**不认**别家的会话格式。技术上补齐就是写两份字段映射（Claude 取 `message.content[*].type`，Codex 取 `response_item` + `payload.type=="message"` 并过滤 `is_codex_context` 注入），再落到 `SessionManager` 的写 API |
| 决定 | 🚫 不实现。理由与翻案条件见 **§3.9** |

### 2.6 WebMCP（Tier 4，🔜）

| 维度 | 内容 |
|---|---|
| 一句话 | 把 Chrome 页面通过实验性 WebMCP API 暴露的工具变成 agent 工具：agent 能直接“点”你正开着的网页上的按钮 / 查它上面的数据，而不是只会抓页面文本 |
| kiss 出处 | `crates/kiss-webmcp/`（`lib.rs` 989 + `client.rs` 170）；`/webmcp [connect|list|disconnect]` |
| kiss 语义（实测） | 常量 `DEFAULT_CDP_PORT=9222`、`CALL_TIMEOUT=60s`、`MAX_TEXT_BYTES=100_000`（`lib.rs:20-22`），client 的 `REQUEST_TIMEOUT=10s`（`client.rs:11`）。连接：给端口时先 GET `http://127.0.0.1:{port}/json/version` 取 `webSocketDebuggerUrl`（`lib.rs:598-621`），直接给 `ws://` 时必须 loopback（`lib.rs:559-597`）。CDP 序列：`Target.setDiscoverTargets` → `Target.getTargets` → `Target.attachToTarget{flatten:true}` → `Page.enable` → `WebMCP.enable`（`lib.rs:113-118`、`314-356`）。调用：`WebMCP.invokeTool{frameId,toolName,input}` 取 `invocationId`，等广播的 `WebMCP.toolResponded`（按 `invocationId` + `sessionId` 匹配），失败/取消时发 `WebMCP.cancelInvocation`（`lib.rs:200-260`），状态分 Completed / Canceled / Error（`:265-280`）。origin 白名单：先 allow 后 deny、**deny 优先**；entry 含 `://` 比 origin，否则比 `host[:port]`（`lib.rs:531-557`），配置源 `settings.webmcp.{allowedOrigins,disallowedOrigins,cdp}`。输出超 100 KB 时按字符边界截断并追加提示（`lib.rs:741-749`）。用户要先在 Chrome 开 `#enable-webmcp-testing` / `#devtools-webmcp-support` 与 `chrome://inspect/#remote-debugging`（README:525-542） |
| prux 现状 | 无（`web_access` 是搜索/抓取，不是“用网页上的工具”） |
| prux 落点 | 扩展 `webmcp`：CDP 客户端用 `tokio-tungstenite 0.30`（`Cargo.toml:160`，带 `connect` + `rustls`；**当前 `src/` 里没有任何调用点**，是现成依赖，不必新增），HTTP 用 `reqwest 0.13`。工具动态注册可行：prux 的 `tools()`（`extensions.rs:407`）**每次组装工具表都会重新问**（`compose_tools`，`agent_session.rs:197`，`Agent::new` 与 `rebuild_tools`（`:1111`）都会调），连上/断开后用 `request_ui(ExtensionUiRequest::RebuildTools)`（`ui.rs:159` / `:181`）触发下一轮重建——所以“页面工具出现在工具表里、断开后消失”能做到。命令 `/webmcp [connect|list|disconnect]`，`settings()` 声明白名单与端口（对齐 kiss 的 `settings.webmcp.*`）；工具名加前缀避免与内建撞名，判定可复用 `core/tool_names.rs` |
| 接缝与约束 | 每帧调用且必须无 I/O 的只有 `overlay_view` / `dock_lines`，那里不能碰 WebSocket；工具调用本身在扩展工具的 handler 里，是正常 I/O。`ws://` 的 loopback 校验用 `utils/net.rs:63` 的 `is_loopback_host` |
| 风险 | 实验性 API（要靠 Chrome flag 打开），协议可能变；“没有 Chrome 就不可用”要写进文档 |
| 验证 | 本地起一个假 CDP WebSocket 服务（就用 `tokio-tungstenite`）回放 kiss 的握手与 `toolResponded` 时序，断言工具注册、调用结果、超时与 100 KB 截断；用 `run_with_timeout` 包住 |
| 工作量 | 中 |

### 2.7 `/recap` 与 `/btw`（Tier 4，✅ 已落地为两个扩展，见 §4.7）

| 维度 | 内容 |
|---|---|
| 一句话 | `/recap` = 输入框上方一行会话小结，空闲自动刷新；`/btw` = 只读旁问，答案弹在输入区上方，**不写会话、不进主对话** |
| `/recap` 用起来是什么样 | 状态区（输入框上方）一行 muted 文本 `※ recap: <≤120 字的小结>`，生成中显示 `<spinner> creating recap...`（`interactive.rs:717-722`）。**默认开启**（`settings.rs:358-359`）：空闲满 5 分钟且不在跑回合就自动重刷（`AUTO_RECAP_IDLE=5min`，`interactive.rs:403`、`1033-1042`），切回终端一眼知道这个会话在干什么。`/recap now`（或空参）立刻刷一次，`/recap on` / `off` 开关，`off` 会取消正在生成的（`interactive.rs:7429-7455`）。小结是“另起一次调用”：只喂最近 12 条、12000 字的转录摘录，要求 ≤120 字单行，`max_tokens=160`，回来再 `normalize_recap` 去前缀并截到 120 字（`session_runner.rs:1341-1370`、`interactive.rs:7833-7847`） |
| `/btw` 用起来是什么样 | `/btw 这个错误码是啥意思` → 输入区上方出现一块 `BTW` 区块：加粗标题 + 缩进的问题 + 空行 + markdown 渲染的答案 + `esc or enter closes`；进行中显示 `answering...`（`esc cancels`），失败显示 `✗ <错误>`；Esc / Enter 关闭，内容不会留在对话里（`interactive.rs:725-747`）。它的“旁”体现在三处：走 `run_ephemeral` 的**空上下文**（`session_runner.rs:1213-1277`）、system prompt 声明只读且**只装 `read` 工具**、`max_tokens=500`，且结果只进 `app.btw_panel`、**不写会话文件**（`answer_btw`，`session_runner.rs:1280-1310`）。用途是“顺手问一句，别污染上下文、别让模型顺着这句话改计划” |
| kiss 出处 | 命令表在 `crates/kiss/src/slash_commands.rs`，分发与渲染在 `modes/interactive.rs`，模型调用在 `kiss-coding/src/session_runner.rs` |
| prux 现状 | 都没有（prux 有 `/rewind`、goal、compaction，但既没有“旁问”也没有“一行小结”） |
| prux 落点（已按此落地为 `btw` / `recap` 两个独立扩展） | 一个扩展（或并入现有小工具扩展）。`/btw`：`on_user_prompt` / `on_user_prompt_with_context`（`extensions.rs:528` / `:537`）返回 `UserPromptAction::Handled`（`:937`）即“认领这条输入、不进 transcript”；一次性模型调用走子代理路径（`extensions/subagent` 的 `manager::dispatch`，可限定工具集与轮次）或虚拟模型（`core/virtual_models`），结果落到 `render_custom_message` / overlay，**不写会话**。`/recap`：同样走一次性子代理，`[now|on|off]` 存扩展 settings；“空闲自动触发”需要一个节拍来源——扩展没有独立定时器接口，可选 `subagent::schedule` 的墙钟实现或 `wants_redraw()`（`extensions.rs:738`）的持续重绘节拍，这是实现时的选型点 |
| 验证 | 假 provider 断言 `/btw` 不产生新的会话 entry（借 `agent_session_path` 那类测试接缝比对会话文件）；`/recap` 的 120 字截断与去前缀写成纯函数单测 |
| 工作量 | 小 |

### 2.8 真实自更新（Tier 4，🚫 决定不实现，见 §3.6）

| 维度 | 内容 |
|---|---|
| kiss 出处 | `crates/kiss/src/update.rs`（`v0.0.35` 为 450 行；`v0.0.18` 是 384 行，增量是措辞重写 `11726b9 style: polish project wording`）：查 GitHub latest release → 下载对应平台归档 → 校验 SHA-256 → 替换自身；`install.sh` / `install.ps1` 同源逻辑 |
| prux 现状 | `extensions/update_check.rs` 只**检查**并提示：`LATEST_API`（`update_check.rs:57`）+ `DOWNLOAD_PAGE`（`:60`），`/update open` 只是打开下载页 |
| 决定 | 🚫 不实现。理由与翻案条件见 **§3.6** |

### 2.9 多账号池与限流切换（Tier 2，🚫 决定不实现，见 §3.7）

| 维度 | 内容 |
|---|---|
| kiss 出处 | `crates/kiss-ai/src/auth/accounts.rs`（697 行）+ `crates/kiss-coding/src/account_failover.rs`（89 行 + 127 行测试）+ `/accounts [list|add|use|remove]` 命令（`crates/kiss/src/modes/interactive.rs` 的 `run_accounts_command`） |
| kiss 语义 | 同一 provider 存多个账号（池）；包装 `StreamFn`：先**扣住** `Start` 事件，若第一个真事件是池内账号的限流错误，就切下一个就绪账号并重发同一请求——agent 循环、转写与会话文件都看不到那次失败尝试 |
| prux 现状 | `core/auth.rs` 每个 provider **只存一份**凭据：`auth.json` 顶层是 `{provider:{type:"api_key"|"oauth",…}}`（`auth.rs:3`），读写在 `read_auth_key`（`:70`）/ `write_auth_key`（`:263`）；没有池、没有账号级限流切换。重试层（`core/provider/retry.rs`）只管请求级重试，不会换凭据 |
| 决定 | 🚫 不实现。理由与翻案条件见 **§3.7** |

## 三、不做成扩展 / 决定不实现（已定案）

本节分两类：**§3.1–§3.4** 是“功能本身不做”（或虽有价值但扩展层够不到，属 core 工程）；
**§3.5–§3.9** 是本轮逐项决策后判为不做、并给出翻案条件的五项。两类都是**决定**，
不是“暂缓”——没有新证据就不要重新启动讨论。

### 3.1 对外接口：ACP / JSONL RPC / WebSocket RPC / 六种语言 SDK

kiss 有原生 ACP（`crates/kiss/src/modes/acp.rs`，1749 行）、JSONL/WS RPC（`modes/rpc.rs`）、
Rust/Python/TS/Node/WASM SDK（`kiss-sdk` 等）。prux 目前没有任何服务端形态。
**为什么不做扩展**：它们都是**新的运行模式**——需要在 `cli/` 选择模式、在 `modes/` 增加入口、
把会话事件序列化出去；扩展只在交互会话内活动（`on_agent_event` 能看到事件，但没法接管进程
的 stdio / 监听端口 / 生命周期）。这是一项 core 工程，不是扩展；价值高（编辑器集成），
但要单独排期。

### 3.2 新 provider 的原生支持（Cursor / Databricks / Snowflake / Bedrock / Copilot）

kiss 把 Cursor 的 HTTP/2 直连、Databricks Unity Gateway、Snowflake Cortex（34 个模型）、
Bedrock、GitHub Copilot 都做进了 `kiss-ai`。
**为什么不做扩展**：provider 适配属于 `core::provider`（prux 的目录数据由
`scripts/sync-models.py` 从 pi 同步，`SUPPORTED_PROVIDERS` 是唯一真源）。
**但有一条扩展路径**：Cursor / Databricks / Snowflake 基本是 OpenAI 兼容或近兼容协议，
用已有的 `proxy` 扩展（本地 OpenAI 兼容网关）做协议适配即可桥接，不必改 provider 层。
真要"像原生一样"再考虑进 core。

**Copilot 不算缺口**：prux 的 `SUPPORTED_PROVIDERS`（`src/core/model_resolver.rs:41`）已有
`github-copilot`，`core/oauth/copilot.rs` 也在——本节首版把 Copilot 与其它四个并列，现在只
Cursor / Databricks / Snowflake / Bedrock 是真的没有。

### 3.3 语音输入（`/voice`）

kiss 的 `crates/kiss/src/voice.rs`（592 行）支持本地 whisper.cpp 与 opt-in 云端转写。
**为什么不做**：本地转写要引入 native 依赖（打包与跨平台成本都高），云端要外发音频；
且 TUI 的输入注入需要 core 配合。成本与收益不匹配。

### 3.4 Jev（TypeSafe）压缩 / 动态推理

kiss 把「压缩策略」与「思考档位选择」外包给第三方服务（`kiss-coding/src/jev.rs`，1090 行），
并明确告知会把上下文发出去。
**为什么不做**：prux 已有 `jet` 扩展（规划/实现双模型虚拟路由）与完整的压缩覆盖点
（`on_before_compact` / `custom_summarize` / `on_boundary`），增量价值取决于用户是否订阅
TypeSafe；愿意用的话完全可以由扩展提供，但优先级低于上面那些。

### 3.5 外部 agent 凭证导入（原 §2.2）

kiss 的 `kiss-ai/src/auth/external.rs`（775 行）能发现并搬运 Codex / Claude Code（含 macOS 钥匙串）/
Pi / OpenCode / OpenClaw / Hermes 的登录态，`kiss auth import` 一条命令搞定。prux 侧技术上做得到
（`write_auth_key` / `write_oauth_credential` / `remove_auth` 都是 `src/core/auth.rs` 的公开 API）。
**为什么不做**：

- **一次性收益 vs 长期维护**：导入只省掉“重登一次”，代价是永久跟着 5 家私有登录态格式走
  （token 结构、路径、钥匙串条目名都会变）——kiss 那份 775 行代码就是这个成本的体现。
- **安全面**：要读别家凭据文件，Claude Code 还要调 `security` 读钥匙串。扩展没有沙箱
  （§1.3：扩展即进程内代码），这类“读任意凭据”的能力不该由一个内置扩展默认持有。
- **不在主线**：pi 没有导入能力，这不是“与 pi 对齐”的一部分；prux 已有
  `--api-key > auth.json > OAuth > models.json > env` 五条登录路径。
- **同源无收益**：从 pi 导入更没意义（格式本来就兼容）。

**翻案条件**：出现“用户因为要重登而放弃 prux”的实证，或 pi 侧先做了同名能力（那时随 pi 契约走）。

### 3.6 真实自更新（原 §2.8）

kiss 的 `update.rs`（450 行）会下载平台归档、校验 SHA-256、替换自身；prux 的
`update_check.rs` 只检查并提示，`/update open` 打开下载页。
**为什么不做**：

- **没有安全的落点**：仓库里没有 `install.sh` / `install.ps1`，发布产物是
  `prux-<version>-x86_64-<os>.tar.gz`（`.github/workflows/{linux,macos,windows}.yml`），
  实际安装形态是“用户自己 `cargo build` 或手动解包”。自替换会让二进制与 release 来源脱钩
  （覆盖掉用户自己构建的那份）。
- **失败代价高于收益**：Windows 替换运行中的 exe 有额外约束，失败会留下半成品安装——
  §2.8 自己也写了“半成品安装比不更新更糟”。
- **收益已被覆盖**：`update-check` 已能发现新版本并一键打开下载页，而 prux 是单二进制，
  手动替换就是几步操作。

**翻案条件**：prux 提供官方安装脚本或走包管理器分发之后，自更新才有安全落点。

### 3.7 多账号池与限流切换（原 §2.9）

kiss 用 `auth/accounts.rs`（697 行）+ `account_failover.rs` 给一个 provider 存多个账号，
限流时换账号重发同一请求。**为什么不做**：

- **存储形状是 pi 契约**：pi 的 `packages/ai/src/auth/types.ts:36` 明写
  「One type-tagged credential per provider — the shape of today's auth.json」，
  prux 的 `auth.json`（`{provider:{type:"api_key"|"oauth",…}}`）与之一致。改成池 = 主动偏离 pi，
  还要迁移老用户的文件（§五那条“老用户升级后直接掉登录”正是这个风险）。
- **不是扩展能做的事**：换账号要重发**同一个请求**并吞掉失败事件，而 `provider_stream_event`
  是只读观察点，拦不住重发；只能改 `core::provider` 的 `StreamFn` 包装层，属 core 工程。
- **收益面窄**：需要“同一 provider 有多个订阅/额度”的用户才用得上；429 在 prux 已有
  `core/provider/retry.rs` 的请求级重试兜底。

**翻案条件**：pi 侧引入账号池（那时随契约实现），或有明确的用户量证据。

### 3.8 实验性「模型自有上下文文件」（原 §6.2 补记）

kiss 的 `kiss-coding/src/context_file.rs` 把会话上下文导出成临时 JSON 让模型改，
改动读回后作为一次 compaction 生效（要显式开 `experimental_context_file`）。
**为什么不做**：

- 读回要往会话里写 compaction（`SessionManager::append_compaction`，`session_manager.rs:1263`），
  而扩展拿到的是不可变的会话视图（如 `on_session_start` 的 `&Session`），没有可变句柄；
- 会话文件的写入在 prux 是单一来源（`SessionManager`），让模型改一份 JSON 再读回等于把上下文
  变成可被外部改写的状态，与 `core/compaction` 的分层冲突；
- 它是 kiss 的实验特性，价值未被验证。

**翻案条件**：kiss / pi 把它做稳并进入正式设置项之后。

### 3.9 跨 agent 会话恢复（原 §2.5）

kiss 的 `session_sources.rs`（434 行）让 `/resume` 能列出并接着用 KISS / Pi / Claude Code / Codex
的会话（`Ctrl+G` 切项目/全局）。prux 侧技术上做得出来（两份字段映射 + 已有的 `/import`），
但**不做**：

- **语义对不齐，导进来只能“看”不能“接着用”**：prux 的会话条目带工具调用、思考块、分支与压缩
  边界，Claude / Codex 的条目模型不同；强行映射出的历史在后续 `/compact`、`/fork`、分支切换上
  行为不确定——半吊子的迁移比“不复用旧历史”更糟。
- **长期维护**：两家都是私有 jsonl，版本一变就得跟着改（同 §3.5 的逻辑）。
- **一次性收益**：换工具只发生一次；真需要旧上下文时，让用户把旧会话导成文本、或让 `/recap`
  式的一次性摘要顶上去就够了，不需要为它写代码。
- **不在主线**：pi 没有这个能力；prux 已有自己的 `/resume` 与 `/import`（自家格式）。

**翻案条件**：Claude Code / Codex 把会话格式文档化并稳定下来，或 pi 侧先做了同样的事。

---

## 四、实施记录

### 4.0 总览

| 小节 | 落地项 | 承载扩展 |
|---|---|---|
| §4.1–§4.4 | §2.1 autoresearch 闭环 | `subagent`（`subagent/autoresearch.rs`） |
| §4.5 | §2.3 缓存效率跨会话报告 | `cache-usage`（新） |
| §4.6 | §2.4 provider 可达性诊断 | `doctor`（新） |
| §4.7 | §2.7 `/recap` + `/btw` | `btw` + `recap`（两个新扩展） |

> 首版这一节记的是一次**并不存在的落地**：按当时的描述去仓里找，`autoresearch.rs`、
> `forget_ctx_for_test()`、那条端到端用例都不存在，`git log --all` 里也没有任何相关提交。
> 本节的每一行都是本次改动的实际情况（可用 `git status` / `git diff` 核对）。

### 4.1 落点

| 文件 | 内容 |
|---|---|
| `src/extensions/subagent/autoresearch.rs`（新增，791 行生产代码 + 470 行测试） | 作业表与状态机、轮次循环、活跃额度、参数解析、提示词、dock 与列表渲染、**16 个单测** |
| `src/extensions/subagent.rs` | `pub(crate) mod autoresearch;`；`commands()` 增加 `/autoresearch`（子命令 `stop`）；`on_registered` 接线 `autoresearch::command`；`dock_lines()` / `wants_redraw()` / `on_enabled_changed(false)` / `on_session_switched` 接入；新增测试接缝 `agent_session_path(id)` |
| `src/extensions/subagent/fleet.rs` | 新增 `forget_ctx_for_test()`（测试接缝：`reset()` 故意保留缓存 ctx，测"还没有上下文"只能显式清） |
| `src/core/agent_session.rs` | 端到端测试 `autoresearch_job_loops_until_the_completion_marker` + 辅助 `await_autoresearch_terminal()`（紧邻既有的 `subagent_extension_runs_agent_tool`，复用同一套假 provider 基建） |
| `assets/changelog/v.1.1.0.md` | 中英各一条（"新增"段） |
| `assets/docs/extensions/subagent.md` | 新增"autoresearch 作业"一节 + 目录项（扩展文档是编译期内嵌的真源） |
| `tests/subagent.rs` | 集成用例 `identity_tool_surface_and_commands` 断言命令表：`cmds.len()` 由 1 改 2，并补上 `/autoresearch` 的子命令断言 |

### 4.2 用法

```text
/autoresearch reduce Markdown render time --iterations 20
/autoresearch                # 列出全部作业（状态 / 轮次 / token / 耗时 / 最近一轮摘要）
/autoresearch stop 3         # 停掉作业 #3（`#3` / `autoresearch-3` 也认）
/agents workflows            # 子代理与工作流运行列表（autoresearch 的子会话在这里）
```

### 4.3 与 kiss 的对齐与偏离

| 项 | kiss | prux 实现 | 说明 |
|---|---|---|---|
| 提示词文案 | `iteration_prompt`（英文） | **逐字对齐** | 完成/继续标记、基线/指标/回退的措辞都照抄；prux 只保留 autoresearch 这一支（kiss 的 `JobKind::Loop` 在 prux 由 `schedule` 覆盖） |
| 完成判定 | `contains_completion_marker`（大小写不敏感） | 同 | 判定留在文本里，不依赖结构化输出 |
| 迭代上限 | 结尾的 `--iterations N`，clamp 100 | 同 | 只认结尾形式（`-n` / `--iterations=N` 会被当成目标的一部分）；`N` 非正整数或后面还有词 → 报错 |
| 并发上限 | 4 个活跃作业、保留 20 条 | 同（`MAX_ACTIVE` / `MAX_RETAINED`） | 第 5 个排队等额度；淘汰只挑已终结的最旧条目 |
| 会话连续性 | 同一子会话多轮（`child_turn` + `ForkTurns::All`） | `manager::dispatch_with_id`（前台）+ `resume` 同一 child；首轮 `inherit_context = true` | 用 prux 已有的 resume 机制表达同一语义（所以每轮都要求 `persist_session`） |
| 无上限模式 | 允许（`--iterations` 可省，靠模型报完成） | 同 | 提示词在无上限时不声称"of N" |
| 暂停/恢复 | `/jobs` 支持 `p` 暂停 | **未做** | prux 侧只有停止；暂停需要"轮间闸门"，等真有需求再加 |
| 作业持久化 | 内存态（重启丢失） | 同（内存态） | 与 kiss 一致；子会话本身落盘，历史不丢 |
| 随主回合中止 | 作业独立于主回合 | `workflow_owned = true` **且每轮换掉 `ctx.parent_abort`** | 只用标志不够：前台路径会经 `run_linked` 跟着父级 `parent_abort` 联动中止，所以每轮都换一个永不置位的信号 |
| 停止路径 | `cancel` token + `child.abort()` | 停止标志 + `manager::stop(child)`，且**派发与停止信号 `select` 竞争** | 否则停止会撞在“记录还没入库”的窗口上停不掉那一轮（子代理会成孤儿跑完） |
| 视图 | `/jobs` 专用面板 | dock 行 + `/autoresearch` 列表；子会话在 `/agents` | 不新造覆盖面板，复用 dock 与既有列表入口 |
| 完成通知 | 靠 `/jobs` 面板自己看（无通知） | 终结时投一条**人向**通知（轮次 / token / 耗时 / 目标，失败带原因）；`Stopped` 不发 | 前台路径本身不发通知（`manager.rs`：「收尾（不发通知）」），而作业终结时 dock 段又会消失——不改就是无声收场。只发通知、不注入 `Continuation`：上百轮的作业若每轮都注入会把主对话灌满（本项为落地后的补充，不在首版记录里） |

### 4.4 验证

```bash
cargo test --lib autoresearch          # 18 passed（17 个单测 + 1 条端到端）
cargo test --lib extensions::subagent  # 288 passed
cargo test --lib core::agent_session   # 97 passed
cargo test                             # 全量绿（lib 3320 + 各集成测试二进制）
cargo clippy --lib --all-targets       # 无新增告警（剩下的都是本次改动前就有的）
```

- **端到端**（`agent_session::tests::autoresearch_job_loops_until_the_completion_marker`）：
  真 `make_sub_agent` + 假 provider 脚本化两轮 SSE —— 第 1 轮子代理回 `first change [continue]`，
  第 2 轮（走 `resume` 的同一子会话）回 `verified [goal-complete]`；断言作业落 `Completed`、
  `iteration == 2`、最后一轮结果含标记，并用 `agent_session_path(agent_id)` 取出子会话文件，
  断言同一个文件里同时有两轮的回答。第二轮能跑起来本身就要求 `resolve_resume` 在真会话文件上成立
  （假 runner 那条用例覆盖不了它）。
- **单元**（`autoresearch::tests`，17 个）：提示词位置/无上限措辞/基线-指标-回退措辞、完成标记大小写、
  参数解析（结尾 `--iterations` / 拒绝 0、非数字、多词、空目标）、id 写法（`3` / `#3` / `autoresearch-3` / 0 / 非数字）、
  无上下文时报错、循环到标记（两轮：第 1 轮无 resume + fork、第 2 轮 resume 同一路径、提示词带 `iteration 1/2`）、
  到上限收工、clamp 100、子代理出错即 `Failed`、停止运行中的作业、`cancel_all` 不动已终结作业、
  dock 只列未终结作业、列表渲染（含失败原因）、终结通知（`Completed` → Success / `Failed` → Warning，
  且断言不产生 `Continuation`；用户停止不发）。
- **未覆盖**：真实模型下的"留优退退化"行为（那由提示词约束，依赖模型质量，无法在测试里断言）；
  真终端里 dock 的观感（dock 行是纯函数，已由单测断言文本）。

---

### 4.5 cache-usage（§2.3 落地）

| 文件 | 内容 |
|---|---|
| `src/extensions/cache_usage.rs`（新增，约 1190 行含测试） | 会话扫描与去重、`Report` 聚合、条形图 / sparkline、覆盖面板与滚动、10 个单测 |
| `src/extensions.rs` | `pub mod cache_usage;` + `pub const PRIORITY_CACHE_USAGE: u32 = 66;` |
| `assets/docs/extensions.md` | 内置扩展表新增 `cache-usage` 行 + `### cache-usage` 一节 |
| `assets/changelog/v.1.1.0.md` | 中英各一条 |
| `tests/extensions_registry.rs` | 扩展计数 20 → 24、补 `cache-usage` 默认不启用断言 |

默认状态：**不启用**，只在 `Dev` / `Creator` 扩展模式下可用（属排查工具，不抢行为）。

用法：`/cache-usage`（当前会话）、`/cache-usage all`（`agent_dir()/sessions` 下全部工程）、
`/cache-usage all <provider>`。面板 = 总量条形图（24 格）+ 32 格命中率趋势 sparkline +
各 provider 分列（≥2 个时）+ 各会话分列（`all` 时按 token 总量取前 12）；
↑↓ / PageUp / PageDown / Home / End 滚动，Esc 关闭；扫描期间显示 `scanning sessions…`。

| 项 | kiss | prux 实现 | 说明 |
|---|---|---|---|
| 数据源 | 己方 session JSONL 的 `SessionManager::entries()` | 同一份会话 jsonl（`Session::open_checked` + `SessionState::entries()`） | 首版设计的"自建 `cache-usage.jsonl`"被否掉：不留第二份真源 |
| 命中率 | `read / (input + read + write)` | 同 | 与 `footer` 底栏同式 |
| 样本 | assistant 消息 + `Compaction` / `BranchSummary` 的 usage | 同 | |
| 去重键 | 条目 id + 时间戳 + provider + 用量 | 同 | fork 出来的会话文件会重复包含父会话条目 |
| 趋势图 | 32×4 累积面图 | 32 格 sparkline（8 档块字符） | 省行数、便于单测；语义相同（左旧右新） |
| CLI | `kiss cache-usage [--session ID\|FILE] [--provider ID]` | **未做** | 扩展只能加 flag、加不了子命令（`src/cli/args.rs` 的 `Command` 只有 `auth` / `mcp`）；要 CLI 形态得进 `src/cli/` |
| 扫描范围 | 全量 | 最新 200 份会话文件 | 防 `all` 在多年历史里卡住（扫描走 `spawn_blocking`，UI 不冻） |
| provider 归属 | 样本自带 provider | 消息的 `provider`；缺失时回退到最近的 `ModelChange` 或上一条带 provider 的 assistant 消息 | 旧会话里 `Compaction` 条目常常没有 provider |

### 4.6 doctor（§2.4 落地）

| 文件 | 内容 |
|---|---|
| `src/extensions/doctor.rs`（新增，约 1030 行含测试） | 探测目标枚举、并发探测、状态/错误分类、明细与汇总渲染、environment 段、13 个单测 |
| `src/extensions.rs` | `pub mod doctor;` + `pub const PRIORITY_DOCTOR: u32 = 64;` |
| `assets/docs/extensions.md` | 表格行 + `### doctor` 一节 |
| `assets/changelog/v.1.1.0.md` | 中英各一条 |
| `tests/extensions_registry.rs` | 补 `doctor` 默认不启用断言 |

默认状态：**不启用**，只在 `Dev` / `Creator` 扩展模式下可用。

用法：`/doctor`（明细）与 `/doctor summary`（计数 + 失败清单）；面板末尾固定附 environment 段。

| 项 | kiss | prux 实现 | 说明 |
|---|---|---|---|
| 探测协议 | SSE `HEAD` / WebSocket `GET` / AWS event-stream / OAuth login 端点，四种分类 | 只探 chat 端点的 `HEAD` | prux 目录里只有 5 种 chat 协议（`anthropic-messages` / `openai-responses` / `openai-completions` / `azure-openai-responses` / `google-generative-ai`），没有 WS / event-stream 形态 |
| 目标范围 | `registry.all()` 全部 provider | `catalog_providers()` 里每个 provider 的第一个 chat 模型 | 同"全覆盖、不按凭据过滤" |
| 凭据 | 请求不带凭据，只有推导 base_url 时读本地凭据 | 完全不读凭据（base_url 来自目录） | |
| 可达判定 | 200 / 401 | **任何 HTTP 响应都算可达**（2xx / 401 / 403 / 405 不加附注，其它标 `status not OK`） | kiss 探的是真实 API 路径，404 于它意味着「端点不对」；prux 探的是 base URL，「路径无处理器」不算网络故障——防火墙白名单要的正是「这个主机出不出得去」 |
| 超时 / 并发 | 10s / 16 | 同（常量照抄） | |
| 输出 | 明细表 + `--summary` 表 + Environment 段 | 覆盖面板（明细 / summary 两档）+ environment 段 | 面板代替 stdout 表 |
| 退出码 | 有 failed → 1 | **无**（TUI 内命令没有退出码语义） | |
| 离线 | 无此概念 | `runtime::offline()` 时不发请求，只显示 environment 段 + `offline mode: network probes skipped` | |
| environment 段 | install / catalog / auth / proxy | 程序名与版本、`agent_dir()`、探测目标数、三个代理变量 set/unset（**不回显值**） | 代理值里可能带凭据 |

### 4.7 btw + recap（§2.7 落地，两个扩展）

| 文件 | 内容 |
|---|---|
| `src/extensions/recap_btw.rs`（新增，模块根） | 声明 `shared` / `btw` / `recap` 三个子模块并说明为什么拆开 |
| `src/extensions/recap_btw/shared.rs`（新增） | 两者共用：会话 id 槽、转录摘录、折行、一次性请求（`spawn_one_shot`）、5 个单测 |
| `src/extensions/recap_btw/btw.rs`（新增） | `btw` 扩展：`/btw` 命令、`BTW` 面板、7 个单测 |
| `src/extensions/recap_btw/recap.rs`（新增） | `recap` 扩展：`/recap` 命令、停靠行、空闲 tick、设置落盘、11 个单测 |
| `src/extensions.rs` | `pub mod recap_btw;` + `PRIORITY_BTW = 63` / `PRIORITY_RECAP = 62` |
| `assets/docs/extensions.md` | 表格两行 + `### btw` / `### recap` 两节 + 默认关闭清单补 `btw`、`recap` |
| `assets/changelog/v.1.1.0.md` | 中英各一条（改动说明里与其它两个新扩展合并成一条） |
| `tests/extensions_registry.rs` | 计数 20 → 24，并加 `btw` / `recap` 默认不启用断言 |

**为什么拆成两个扩展**：`/btw` 只在用户显式发起时才有动作（不问就不花钱），而 `/recap`
开启后会在**空闲时自动**发起请求（要花 token）。把两个开关绑在一起，等于"想用旁问就得接受
后台自动花钱"，是把两种不同代价的行为捆在一块。拆开后各自的 `default_enabled()` 与
扩展设置也互不干扰；共用逻辑（摘录 / 折行 / 一次性请求）放在同模块的 `shared.rs` 里，
不会分叉成两套语义。

默认状态：两个都**不启用**，只在 `Dev` / `Creator` 扩展模式下可用。

用法：`/btw <问题>`、`/recap [now|on|off]`。

| 项 | kiss | prux 实现 | 说明 |
|---|---|---|---|
| `/btw` 上下文 | `transcript_excerpt(4 条, 4000 字)` + 只装 `read` 工具 | 同样 4 条 / 4000 字，**不给任何工具** | 一次性请求本来不带工具，安全面更小 |
| `/btw` 执行 | `run_ephemeral`（空上下文新开一次） | `provider::simple_completion`（非流式一次性请求） | 都不写会话文件 |
| `/btw` 展示 | `app.btw_panel`（输入区上方，esc / enter 关闭） | `OverlaySize::Panel` 覆盖面板：占据输入区（主页输入框隐藏）、自带单行 `btw>` 输入框且打开即聚焦，可连续追问（Esc / 空回车关闭） | kiss 显示在输入区上方；prux 用 Panel 档隐藏主页输入框并复用覆盖层的输入行，避免“看得见却打不了字” |
| `/recap` 展示 | 状态区一行 `※ recap: …` | 停靠面板（dock）一行 | dock 就是"状态栏上方"，语义等价 |
| `/recap` 生成 | 12 条 / 12000 字摘录，≤120 字，`max_tokens=160` | 同，另做前缀/引号清洗 + 硬截断（省略号不占配额） | |
| 空闲触发 | TUI 事件循环判 5 分钟 | 30 秒 tick 后台任务（"代际"退出，同 `subagent::schedule`） | 扩展没有独立定时器接口 |
| 默认开关 | 默认开启 | **整个扩展默认不启用**；启用后 `autoRecap` 默认 on，可关 | 自动小结要花 token，不该由框架替用户决定 |
| 设置落盘 | kiss settings | `agent_dir()/extensions/recap.json` 的 `autoRecap`，也可在扩展设置面板改 | 扩展自己的配置文件 |
| 模型选择 | kiss 用会话当前模型 | 当前 provider/model；兜底取摘录里最后一条带 `provider`/`model` 的 assistant 消息 | 不引入虚拟模型 |

### 4.8 验证（§4.5–§4.7）

```bash
cargo test --lib cache_usage             # 10 passed
cargo test --lib doctor                  # 13 passed
cargo test --lib recap_btw               # 23 passed（shared 5 + btw 7 + recap 11）
cargo test --test extensions_registry    # 2 passed（计数 20 → 24、注册序、默认启用断言）
cargo test                               # 全量绿（lib 3361 + 各集成测试二进制）
cargo clippy --all-targets               # 新代码零告警（残留告警都在本次改动之前）
cargo fmt --check                        # 通过
```

- **cache-usage**：条形成比例、命中率口径（含 0 分母）、sparkline 分档与左侧补位、参数解析
  （`all` / provider / 两个 provider 报错）、真实临时会话文件的聚合与 fork 去重、provider 过滤、
  报告行文本、面板认领与滚动封顶、禁用清状态。
- **doctor**：`host[:port]` 解析（含 userinfo / 端口 / IPv6 方括号）、状态码分类
  （2xx / 401 / 403 / 405 可达，404 / 500 不可达）、错误分类（超时 vs 连接失败）、
  detail / summary / environment 行文本（含"只显示 set/unset"）、未知子命令提示、
  面板认领与滚动、禁用清状态；**本地 TcpListener** 覆盖 200 / 401 / 挂起 / 连接拒绝四种真实探测
  （`run_with_timeout` 包住，全程不联网）。
- **shared**（5）：摘录条数 / 字符上限 / 空文本跳过、`(provider, model)` 兜底提取、折行、
  TUI 镜像优先于快照、无 tokio 运行时时 `spawn_one_shot` 返回错误而不是 panic。
- **btw**（7）：无问题提示用法、无型号提示、正常开面板（面板文本含问题与错误）、迟到答案在面板
  关闭后丢弃、禁用清状态、命令与模式声明（`Dev` / `Creator` + 默认不启用）、会话切换收面板。
- **recap**（11）：前缀/引号清洗与硬截断、空闲判定矩阵、停靠行 spinner → 小结文本、
  `/recap on|off` 落盘与非法子命令、`now` 在缺型号 / 缺历史时提示、设置面板读值与
  `apply_setting` 校验、`transform_context` 快照、禁用清状态、命令与模式声明、会话切换复位
  但保留设置。
- **未覆盖**：真实模型下 `/btw` / `/recap` 的回答质量（取决于模型，无法断言）；真终端里
  dock 与面板的观感（行文本已由单测断言）。`all` 模式对真实历史会话的耗时（本次只在
  临时目录的小样本上测过）；`/doctor` 只验证了本地测试服务与三个真实端点（本机网络受限，
  没能逐一核对 35 个 provider 的 `HEAD` 行为）；四个新扩展在 `Dev` / `Creator` 模式下的
  面板可见性只由 `tests/extensions_registry.rs` 的注册表断言覆盖，没在真 TUI 里点过。
- **已知限制**：自动小结的摘录快照是在 `transform_context`（每次 LLM 调用前）刷新的，
  而该钩子不带 `agentScope`——主 agent 与子代理的调用会共享这份快照，最后一次写入者生效。
  手动 `/recap` 与 `/btw` 不受影响：它们直接读 `App.messages`。

## 五、落地后的收尾检查

1. **渲染与 I/O 的边界**：三个新扩展都守住了——覆盖面板 / 停靠行只画缓存在 state 里的数据，
   读盘、聚合、网络探测分别在命令执行时、`spawn_blocking` 或 `spawn` 里完成（§4.5–§4.7）。
2. **changelog**：`assets/changelog/v.1.1.0.md` 中英各加三条（本仓规定缺文件会编译失败）。
3. **扩展文档**：`assets/docs/extensions.md` 加了四行表格 + 四节说明，并把 `cache-usage`、
   `doctor`、`btw`、`recap` 都登记进"默认关闭"清单。
4. **测试**：新增代码共 46 个单测（cache-usage 10 / doctor 13 / btw 7 / recap 11 / shared 5），
   另有 `tests/extensions_registry.rs` 的注册计数与顺序断言随扩展数更新（20 → 24），
   并逐个断言四个新扩展**默认不启用**。
5. **默认状态**：四个新扩展都是 `default_enabled() == false` + `modes() == [Dev, Creator]`，
   与 `hang-detect` / `notify` / `rewind` / `jet` / `codemode` 等既有可选扩展同一档。

---

## 六、重新评估记录（kiss v0.0.18 → v0.0.35）

### 6.1 首版里必须纠正的错误

| 位置 | 首版写的 | 实际 |
|---|---|---|
| 头部版本行 | 检出是 `0.0.18`，文件名里的 `0.0.35` 只是任务编号 | 现检出 `v0.0.35-5-gdd29d0d`，版本与文件名一致；对齐的 pi 是 `v0.99.1`（首版写的 `v0.87.1` 已过期） |
| §4 实施记录 | 记了 `autoresearch.rs`、`forget_ctx_for_test()`、端到端用例与"11 passed" | **当时全都不存在**（无文件、无 git 历史）。本次真落地，§4 已重写 |
| §4.3 参数形式 | 声称 `-n` / `--iterations=N` 也认 | kiss 只认**结尾**的 `--iterations N`；prux 同样只认这一种（不额外放宽，以免同一串输入在两边的语义不同） |
| §2.8 行数 | `update.rs` 384 行 | `v0.0.35` 是 450 行（`v0.0.18` 确实是 384；增量来自 `11726b9`） |
| §2.6 行数 | webmcp "约 700 行" | `lib.rs` 989 + `client.rs` 170 |
| §2.4 输出 | 探测结果里出“防火墙白名单”表 | kiss **不生成**白名单（`doctor.rs` 无此逻辑，README:310-311 只是建议把失败行交给网络团队）；本轮已按实测改写 §2.4 |

其余行数（`cache.rs` 458、`doctor.rs` 732、`session_sources.rs` 434、`jev.rs` 1090、`subagents.rs` 917、
`external.rs` 772@v0.0.18）逐一核对无误。

### 6.2 `v0.0.18` → `v0.0.35` 与本档相关的变更

先按文件确认"判断是否失效"：

| kiss 文件 | 变动 | 对本档的影响 |
|---|---|---|
| `kiss-coding/src/iterative.rs`、`kiss/src/job_ui.rs` | **未改** | §2.1 的语义描述与 §4.3 的对齐表仍然成立（autoresearch 是本次落地的基准） |
| `kiss/src/cache.rs`、`doctor.rs`、`session_sources.rs`、`kiss-coding/src/jev.rs`、`kiss-coding/src/subagents.rs` | **未改** | §2.3 / §2.4 / §1.2 的对照仍然成立；§2.5 的事实成立，但结论已改为不实现（§3.9） |
| `kiss-ai/src/auth/external.rs` | +3 行 | §2.2 的事实成立，但结论已改为不实现（§3.5） |
| `kiss/src/modes/acp.rs` | +25 行 | §3.1 的结论（新运行模式，不是扩展）不变 |
| `kiss-webmcp/src/lib.rs` | +5 行 | §2.6 成立 |
| `kiss/src/update.rs` | 措辞重写（384→450） | §2.8 的事实成立，但结论已改为不实现（§3.6） |

新增能力（首版清单里没有，重核时补入）：

| kiss 新增 | 是什么 | 判断 |
|---|---|---|
| `kiss-ai/src/auth/accounts.rs` + `kiss-coding/src/account_failover.rs` + `/accounts` | 同一 provider 多账号池，限流时切换并重发 | 真缺口，补入 **§2.9**；后续判为不实现（§3.7） |
| `kiss-coding/src/context_file.rs` | 实验性"模型自有上下文"：把会话上下文导出到临时 JSON，模型可改，改动读回后作为一次 compaction 生效（设 `experimental_context_file` 开启） | **🚫 决定不实现**，理由与翻案条件见 §3.8 |
| `kiss/src/terminal_hosts.rs` + OSC 7501 | 向终端宿主上报程序状态（宿主 IPC 命令队列） | **已覆盖**：prux 已有 OSC 7501 程序状态（`modes/interactive/program_status.rs`），机制不同（直报终端 vs 转发宿主），不需要移植 |
| `/voice`、`/config voice-language` | 本地/云端语音听写 | 已在 §3.3 定案不做；这里只是补记它们已是正式斜杠命令 |
| 项目信任门移除（`refactor!: remove the project trust gate`） | kiss 删掉了项目信任门 | 与 prux 的 pi 契约无关：prux 的项目信任对齐的是 pi，不跟 kiss 走 |

### 6.3 重核后不变的结论

- §1.2 的"已覆盖"对照、§1.3 的扩展能力边界（可注册/可画 UI/可挂钩子/不能改 provider 层）都没变；
- §3 的四项"不做成扩展"（ACP/RPC/SDK、新 provider、语音、Jev）依旧成立，理由也依旧成立；
- §2.3 / §2.4 / §2.7 的落点与估计在落地时基本成立（§4.5–§4.7）；§2.2 / §2.5 / §2.8 / §2.9
  与 §6.2 的上下文文件判为不实现，理由与翻案条件集中在 §3.5–§3.9。
