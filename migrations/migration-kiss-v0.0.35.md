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
> **目标**：`src/extensions/` 下的扩展（不与上游插件移植混在一起；本仓已有
> `migration-subagents.md` / `migration-tasks.md` / `migration-kiss-plugins.md`）。
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

按 `v0.0.35` 重核后，真正的差集落在三处：

1. **一款 kiss 招牌能力**：autoresearch 闭环（§2.1，Tier 1，**本次已落地**，见 §4）；
2. **一整类「可观测性与运维」能力**：凭证导入、缓存效率趋势、网络诊断、跨 agent 会话恢复、
   真实自更新（§2.2–§2.8，Tier 2–4，待做）；
3. **多账号池与限流切换**（§2.9，Tier 2）：`v0.0.19` 之后新加的能力，首版清单里没有它。

另有三类**不该做成扩展**（§3）：对外接口（ACP / RPC / SDK）、provider 层新增（Cursor /
Databricks / Snowflake / Bedrock）、语音输入。前两类需要进程级接线与 `core::provider` 改动，
扩展层够不到；第三类成本与收益不匹配。

### Tier 路线图

| Tier | 项 | 状态 |
|---|---|---|
| 1 | §2.1 autoresearch 闭环 | ✅ 已落地（§4，本次） |
| 2 | §2.2 外部 agent 凭证导入、§2.3 缓存效率跨会话报告、§2.9 多账号池与限流切换 | 🔜 待做 |
| 3 | §2.4 provider 可达性诊断、§2.5 跨 agent 会话恢复 | 🔜 待做 |
| 4 | §2.6 WebMCP、§2.7 `/recap` + `/btw`、§2.8 真实自更新 | 🔜 待做 |
| — | §3 「不做扩展」四项 | 🚫 已定案 |

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

### 2.2 外部 agent 凭证导入（Tier 2，🔜）

| 维度 | 内容 |
|---|---|
| kiss 出处 | `crates/kiss-ai/src/auth/external.rs`（`v0.0.35` 为 775 行，较 `v0.0.18` +3）：发现并导入 OpenAI Codex、Claude Code（含 macOS 钥匙串）、Pi、OpenCode、OpenClaw、Hermes 的登录态；`crates/kiss/src/auth_flow.rs` 的 `kiss auth import` |
| prux 缺口 | 只有 `auth.json` + OAuth 登录链路（`--api-key > auth.json > OAuth > models.json > env`），**没有**从其它 agent 导入 |
| prux 落点 | 扩展 `credentials-import`：读别人的凭据文件 → 写入 prux 的 `auth.json`；命令 `/credentials-import`（或 CLI 子命令 + 扩展 flag） |
| 需确认的接缝 | `core/auth.rs` 里 `auth.json` 的写入**已经是公开 API**（`write_auth_key` / `write_oauth_credential`，见 `src/core/auth.rs`）——扩展不必自己拼盘，直接调它们即可，这一点首版写成“待确认”，现已确认 |
| 价值 | 高：迁移用户的第一次体验常常卡在"还要重新登录一遍"。从 pi 导入的价值低（同源、格式大概率已兼容），价值主要在 Claude Code / Codex |
| 工作量 | 中（读取 + 格式映射 + 校验；macOS 钥匙串要 `security` 命令） |

### 2.3 缓存效率跨会话报告（Tier 2，🔜）

| 维度 | 内容 |
|---|---|
| kiss 出处 | `crates/kiss/src/cache.rs`（458 行）：`/cache-usage` 条形图；`all` 跨会话、`<provider>` 跨 provider；CLI `kiss cache-usage --provider/--session` |
| prux 现状 | **只有当前会话**：`extensions/footer.rs` 的 `latest_cache_hit_rate`（`CH%`）+ 累计 `cache_read` / `cost`；`core/cache_warmer.rs` 只管"要不要保温" |
| prux 落点 | 扩展 `cache-usage`：`on_agent_event` / `on_session_start(&[AgentMessage])` 里读 `Usage`（`cache_read` / `cache_write` / `input` / `cost.total`）→ 追加写 `agent_dir()/extensions/cache-usage.jsonl` → `/cache-usage` 命令 + `overlay_view` 画图 |
| 价值 | 中高：长会话的账单主要就是缓存命中率，而"这个 provider 值不值得开保温"只能靠跨会话趋势回答 |
| 工作量 | 小-中（数据就是 `Usage`；渲染照 `footer` 的表格/条形图做法） |

### 2.4 provider 可达性诊断（Tier 3，🔜）

| 维度 | 内容 |
|---|---|
| kiss 出处 | `crates/kiss/src/doctor.rs`（732 行）：`kiss doctor [--summary]`，16 并发探测每个 provider 的 SSE / WebSocket / AWS event-stream / login 端点，出"防火墙白名单"表；不发送 prompt、不动凭据 |
| prux 缺口 | 无网络诊断能力（`core/bug_report.rs` 只收集崩溃与失败轮次的诊断信息） |
| prux 落点 | 扩展 `doctor`：读 `agent_dir()/models.json`（每条模型带 `baseUrl` / `api`，已在 `assets/models/models.json` 里确认）与 `assets/models/providers.json`（35 个 provider）→ 并发探测 → `/doctor` 命令 + 覆盖面板/自定义消息卡 |
| 价值 | 中：企业网络下的排障利器，也是"到底是 provider 挂了还是我的网挂了"的判据 |
| 工作量 | 中（探测本身简单，工作量在端点分类与输出整形） |

### 2.5 跨 agent 会话恢复（Tier 3，🔜）

| 维度 | 内容 |
|---|---|
| kiss 出处 | `crates/kiss/src/session_sources.rs`（434 行）：`/resume` 能读 KISS / Pi / Claude Code / Codex 的会话，`Ctrl+G` 切项目/全局 |
| prux 现状 | 有 `/resume`（自己的会话）与 `/import <path>`（自己的 jsonl），**不认**别家的会话格式 |
| prux 落点 | 两层选择：① 扩展做格式转换（Claude Code / Codex 会话 → prux jsonl）再交给已有的 `/import`，改动面最小；② 直接接管 `/resume` 的选择器，需要 core 的会话创建接口 |
| 价值 | 中高（换工具时最痛的一步）；工作量 中（转换器本身机械，难在别家格式的版本漂移） |

### 2.6 WebMCP（Tier 4，🔜）

| 维度 | 内容 |
|---|---|
| kiss 出处 | `crates/kiss-webmcp/`（`lib.rs` 989 + `client.rs` 170；首版写的“约 700 行”是旧数）：直连 Chrome 调试端口，把页面通过实验性 WebMCP API 暴露的工具变成 agent 工具（list / describe / call，带 origin 白名单与 100 KB 输出上限）；需要 Chrome flag |
| prux 缺口 | 无（`web_access` 是搜索/抓取，不是"用网页上的工具"） |
| prux 落点 | 扩展 `webmcp`：自建 CDP WebSocket 客户端 + `tools()` 动态注册 |
| 价值 | 低-中（实验性、要求用户改 Chrome flag），但差异化明显 |
| 工作量 | 中 |

### 2.7 `/recap` 与 `/btw`（Tier 4，🔜）

| 维度 | 内容 |
|---|---|
| kiss 出处 | `crates/kiss/src/slash_commands.rs` 的本地命令表：`/recap`（单行会话小结）、`/btw`（只读旁问，不改上下文） |
| prux 缺口 | 都没有（prux 有 `/rewind`、`goal`、`compaction`，但无"旁问"与"一行小结"） |
| prux 落点 | 一个小扩展（或并入现有扩展）：`/btw` 走 `on_user_prompt` → `Handled`（不进 transcript）；`/recap` 用一个虚拟模型/一次性子代理出单行摘要 |
| 价值 | 低（习惯养成型功能）；工作量 小 |

### 2.8 真实自更新（Tier 4，🔜）

| 维度 | 内容 |
|---|---|
| kiss 出处 | `crates/kiss/src/update.rs`（`v0.0.35` 为 450 行；`v0.0.18` 是 384 行，增量是措辞重写 `11726b9 style: polish project wording`）：查 GitHub latest release → 下载对应平台的归档 → 校验 SHA-256 → 替换自身；`install.sh` / `install.ps1` 同源逻辑 |
| prux 现状 | `extensions/update_check.rs` 只**检查**并提示，`/update open` 打开下载页 |
| prux 落点 | 扩展 `self-update`：下载 + 解包 + 原子替换（需要引入 `self-replace` 这类依赖，或自己写 rename 交换 + 保留旧文件回滚） |
| 风险 | 中-高：Windows 上替换运行中的 exe 有额外约束；替换失败会留下半成品安装。建议先做"下载到侧目录 + 打印替换命令"，把原子替换留作后续 |
| 价值 | 中；工作量 中 |

### 2.9 多账号池与限流切换（Tier 2，🔜，重核时新增）

| 维度 | 内容 |
|---|---|
| kiss 出处 | `crates/kiss-ai/src/auth/accounts.rs`（697 行）+ `crates/kiss-coding/src/account_failover.rs`（89 行 + 127 行测试）+ `/accounts [list|add|use|remove]` 命令（`crates/kiss/src/modes/interactive.rs` 的 `run_accounts_command`） |
| kiss 语义 | 同一 provider 存多个账号（池）；包装 `StreamFn`：先**扣住** `Start` 事件，若第一个真事件是池内账号的限流错误，就切下一个就绪账号并重发同一请求——agent 循环、转写与会话文件都看不到那次失败尝试 |
| prux 缺口 | `core/auth.rs` 每个 provider **只存一份**凭据（`write_auth_key` / `read_auth_key`，`auth.json` 键即 provider）；没有池、没有账号级限流切换。重试层（`core/provider/retry.rs`）只管请求级重试，不会换凭据 |
| prux 落点 | 两层：① `core/auth.rs` 的存储从"一 provider 一份"扩成"一 provider 一份列表"（**动 core**，且要兼容既有 `auth.json`）；② 包装层（`StreamFn` 包装、扣 `Start` 事件、限流时换账号）落在 `core/provider` 或扩展的 `provider_stream_event` 钩子上 |
| 为什么不是纯扩展 | 换账号要重发**同一个请求**并吞掉失败事件，而 `provider_stream_event` 是只读观察点，拦不住重发；`proxy` 扩展能做协议适配，但换不了 prux 自己的登录态 |
| 价值 | 中-高：有多个订阅/额度账号的用户遇到 429 直接掉回合，这是刚需；工作量 中-大（存储迁移 + 流包装） |

---

## 三、不做成扩展（已定案）

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

---

## 四、实施记录：autoresearch（本次真正落地）

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

### 4.4 验证

```bash
cargo test --lib autoresearch          # 17 passed（16 个单测 + 1 条端到端）
cargo test --lib extensions::subagent  # 287 passed
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
- **单元**（`autoresearch::tests`，16 个）：提示词位置/无上限措辞/基线-指标-回退措辞、完成标记大小写、
  参数解析（结尾 `--iterations` / 拒绝 0、非数字、多词、空目标）、id 写法（`3` / `#3` / `autoresearch-3` / 0 / 非数字）、
  无上下文时报错、循环到标记（两轮：第 1 轮无 resume + fork、第 2 轮 resume 同一路径、提示词带 `iteration 1/2`）、
  到上限收工、clamp 100、子代理出错即 `Failed`、停止运行中的作业、`cancel_all` 不动已终结作业、
  dock 只列未终结作业、列表渲染（含失败原因）。
- **未覆盖**：真实模型下的"留优退退化"行为（那由提示词约束，依赖模型质量，无法在测试里断言）；
  真终端里 dock 的观感（dock 行是纯函数，已由单测断言文本）。

---

## 五、后续动手前的检查项

1. **`auth.json` 的写入路径**（§2.2）：已确认 `core/auth.rs` 有公开写入 API
   （`write_auth_key` / `write_oauth_credential` / `remove_auth`）——扩展直接调即可，不必自拼盘。
   真正要当心的是"导入的凭据要不要覆盖既有登录态"（建议只在目标 provider 尚未登录时写，或让用户确认）。
2. **别家会话格式的版本漂移**（§2.5）：Claude Code / Codex 的 jsonl 结构会变，转换器要
   容错并在解析失败时明确报"哪个字段没认出来"，不要静默丢消息。
3. **自更新的原子性**（§2.8）：先做"下载 + 校验 + 打印替换命令"，把原子替换留到有跨平台
   验证手段之后——半成品安装比不更新更糟。
4. **多账号的存储迁移**（§2.9）：改 `auth.json` 形状时要兼容既有单凭据格式，
   否则老用户升级后直接掉登录。
5. **changelog**：每次落地都要在 `assets/changelog/v.<Cargo.toml version>.md` 里记一笔
   （缺文件会编译失败；`core::changelog::VERSION` 取自 `Cargo.toml`）。

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

其余行数（`cache.rs` 458、`doctor.rs` 732、`session_sources.rs` 434、`jev.rs` 1090、`subagents.rs` 917、
`external.rs` 772@v0.0.18）逐一核对无误。

### 6.2 `v0.0.18` → `v0.0.35` 与本档相关的变更

先按文件确认"判断是否失效"：

| kiss 文件 | 变动 | 对本档的影响 |
|---|---|---|
| `kiss-coding/src/iterative.rs`、`kiss/src/job_ui.rs` | **未改** | §2.1 的语义描述与 §4.3 的对齐表仍然成立（autoresearch 是本次落地的基准） |
| `kiss/src/cache.rs`、`doctor.rs`、`session_sources.rs`、`kiss-coding/src/jev.rs`、`kiss-coding/src/subagents.rs` | **未改** | §2.3 / §2.4 / §2.5 / §1.2 的对照仍然成立 |
| `kiss-ai/src/auth/external.rs` | +3 行 | §2.2 成立 |
| `kiss/src/modes/acp.rs` | +25 行 | §3.1 的结论（新运行模式，不是扩展）不变 |
| `kiss-webmcp/src/lib.rs` | +5 行 | §2.6 成立 |
| `kiss/src/update.rs` | 措辞重写（384→450） | §2.8 成立 |

新增能力（首版清单里没有，重核时补入）：

| kiss 新增 | 是什么 | 判断 |
|---|---|---|
| `kiss-ai/src/auth/accounts.rs` + `kiss-coding/src/account_failover.rs` + `/accounts` | 同一 provider 多账号池，限流时切换并重发 | 真缺口，已补入 **§2.9**（Tier 2） |
| `kiss-coding/src/context_file.rs` | 实验性"模型自有上下文"：把会话上下文导出到临时 JSON，模型可改，改动读回后作为一次 compaction 生效（设 `experimental_context_file` 开启） | **暂不列项**：读回需要 `SessionManager` 的写入口（`append_compaction`），扩展够不到；而且它是实验特性，价值待观察 |
| `kiss/src/terminal_hosts.rs` + OSC 7501 | 向终端宿主上报程序状态（宿主 IPC 命令队列） | **已覆盖**：prux 已有 OSC 7501 程序状态（`modes/interactive/program_status.rs`），机制不同（直报终端 vs 转发宿主），不需要移植 |
| `/voice`、`/config voice-language` | 本地/云端语音听写 | 已在 §3.3 定案不做；这里只是补记它们已是正式斜杠命令 |
| 项目信任门移除（`refactor!: remove the project trust gate`） | kiss 删掉了项目信任门 | 与 prux 的 pi 契约无关：prux 的项目信任对齐的是 pi，不跟 kiss 走 |

### 6.3 重核后不变的结论

- §1.2 的"已覆盖"对照、§1.3 的扩展能力边界（可注册/可画 UI/可挂钩子/不能改 provider 层）都没变；
- §3 的四项"不做成扩展"（ACP/RPC/SDK、新 provider、语音、Jev）依旧成立，理由也依旧成立；
- §2.2–§2.8 的优先级与工作量估计不变（除了 §2.9 是新补的 Tier 2）。
