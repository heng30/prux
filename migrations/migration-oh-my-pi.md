# Prux 迁移指南：oh-my-pi → prux 扩展（候选清单）

> **来源**：`oh-my-pi`（`omp`，`https://github.com/can1357/oh-my-pi`，pi-mono 的 TS + Rust 大分叉；
> 本仓的本地只读检出是 `tmp/oh-my-pi -> /home/blue/wayshot-fork/oh-my-pi`，检出点
> `[package] version = "18.12.2"`（`packages/coding-agent/package.json`，CHANGELOG 最新条目
> `## [18.12.2] - 2026-10-18`））。自述规模：60+ provider、31 个内置工具、14 个 LSP op、
> 28 个 DAP op、`crates/` 下 13 个 Rust crate（`README.md` 头部）。
>
> **目标**：`src/extensions/` 下的内置扩展（不与「pi 版本对齐」混在一起；本仓已有
> `migration-v*.md`（pi 契约对齐）、`migration-subagents.md` / `migration-tasks.md`（上游插件整体移植）、
> `migration-kiss-v0.0.35.md` / `migration-jcode-v0.94.0.md`（同源 Rust 实现的功能差集））。
>
> **性质**：先冻结「值得移植 / 不值得移植」的判断，再逐条落地；每落地一项，更新路线图状态
> 并在该节末尾补一块「落地记录」。本档首版**全部为候选**，不含任何已落地项。
>
> **判断口径（本档的尺子）**：一个东西算「可作为扩展」，当且仅当它是**一项独立功能、
> 自带用户可见价值**，且能只靠 `src/core/extensions.rs` 的 `Extension` trait 面
> （工具 / 命令 / 快捷键 / hook / dock / overlay / settings / virtual_models / cli_flags）实现。
> 满足以下任一条的**一律明确不做**：
> 1. 它是**机制 / 策略**而不是功能（hooks、审批门禁、内部 URL scheme 注册表、provider 路由策略）；
> 2. 它是**某个已有扩展的辅助项**（应并进那个扩展，而不是新起一个）；
> 3. 它需要**新运行模式 / 服务端 / 守护进程 / 新 provider 协议 / 改 core**，扩展层够不到。
>
> **与既有迁移文档的关系**：oh-my-pi 与 prux 同源（都对齐 pi 契约），所以本档与
> `migration-jcode-v0.94.0.md` 是同一类文档：记的是**两套实现之间的功能差集**。
> 凡是 pi 契约本身的差异，以 prux 侧的 `migration-v*.md` 为准；jcode / kiss 已定案的
> 重依赖项（embedding 栈、多账号、远程客户端、外部 shell hooks、市场）**沿用其结论**，
> 本档只补 oh-my-pi 侧的额外证据。
>
> **⚠️ 一处必须记住的体量差**：oh-my-pi 不是一个和 prux 等量级的实现。
> prux 是单 crate、`src/extensions.rs` 下 32 个扩展模块（footer 三布局单列）；oh-my-pi 是 Bazel + Bun workspace，
> `packages/` 下 18 个包 + `crates/` 下 13 个 Rust crate，`pi-builtins` 单 crate 就 11 万行。
> 因此本档里「不做」的比例会明显高于 jcode / kiss 两份——那不是保守，是对象不同。

---

## 〇、摘要

### 结论

oh-my-pi 与 prux 的重叠面比 kiss / jcode 更小，但**差集的方向是一致的**：模型循环、
子代理、MCP、技能、压缩、缓存保温、联网检索、浏览器、本地 OpenAI 兼容网关这些「自动化那一半」
prux 都已有，对照见 §2。oh-my-pi 真正多出来的是三类东西：

1. **把外部世界搬进工具面**（文档转换、外部 agent 配置导入、GitHub CLI、桌面、LSP / DAP）；
2. **对上下文做手术**（TTSR 流中打断、`/shake` 手动减重、`/handoff` 就地压缩）； 
3. **Rust 侧的重型基建**（进程内 shell + 71 个 coreutils、gix/jj、ast-grep、隔离层）。

按判断口径逐项过筛后，能作为独立扩展的共 **15 项**（§3），其中第一梯队 6 项、
第二梯队 6 项、第三梯队（仅记录、暂不落地）3 项；**其余全部明确不做**（§4）。
按「性价比 × 与 prux 缺口的相关性」排序，最该先做的是
**`ast-grep`（结构化搜索/改写）**、**`agent-import`（八种外部配置导入）**、
**`markit`（docx/pptx/xlsx/epub → Markdown）**、**`shake`（上下文减重）**。

### 路线图

| Tier | 主题 | 项 | 状态 |
|---|---|---|---|
| **Tier1** | 结构化检索 | §3.1 `ast-grep`（`ast_grep` + `ast_edit`） | 🟡 候选（推荐先做） |
| **Tier1** | 生态互通 | §3.2 `agent-import`（八种外部 agent 配置导入） | 🟡 候选（推荐先做） |
| **Tier1** | 文档读取 | §3.3 `markit`（文档 → Markdown，并把 `pdf` 扩展并入） | 🟡 候选（推荐先做） |
| **Tier1** | 上下文手术 | §3.4 `shake`（手动丢弃重内容） | 🟡 候选（推荐先做） |
| **Tier1** | 交互 | §3.5 `ask`（结构化提问工具） | 🟡 候选（需先验证回传通道） |
| **Tier1** | 技能沉淀 | §3.6 `manage_skill`（写 SKILL.md，并入 `skills` 扩展） | 🟡 候选 |
| **Tier2** | 上下文手术 | §3.7 `handoff`（交接文档 + 就地压缩） | 🟡 候选 |
| **Tier2** | 多模态 | §3.8 `generate_image`（图像生成工具） | 🟡 候选 |
| **Tier2** | 协作 | §3.9 `advisor`（第二模型旁观每轮） | 🟡 候选（中等成本） |
| **Tier2** | 质量 | §3.10 `cleanse`（检查器驱动的批量修复） | 🟡 候选（依赖 `subagent`） |
| **Tier2** | 提示词 | §3.11 `magic-keywords`（`ultrathink` / `orchestrate` / `workflowz`） | 🟡 候选（`ultrathink` 需 core 配合） |
| **Tier2** | 会话模式 | §3.12 `loop`（同一提示词反复重投直到闸门放行） | 🟡 候选（便宜：纯 `Boundary`） |
| **Tier3** | 联网 | §3.13 `github`（gh CLI 封装） | 🟡 仅记录 |
| **Tier3** | 运行环境 | §3.14 `python`（持久 Python cell，并入 `codemode`） | 🟡 仅记录 |
| **Tier3** | 代码智能 | §3.15 `lsp`（只读查询半套） | 🟡 仅记录（工作量大） |
| 不做 | §4.1–§4.16 | 16 组 | 🚫 明确不做 |

---

## 一、架构对照

### 1.1 两仓的模块形状

| 维度 | oh-my-pi | prux |
|---|---|---|
| 顶层 | Bun workspace + Bazel；`packages/`（coding-agent / ai / tui / natives / …）+ `crates/`（13 个 Rust crate） | 单 crate（`src/` 分层，见 `AGENTS.md`） |
| 语言划分 | 业务在 TS/Bun，性能热点在 Rust crate，经 `pi-natives` 的 N-API cdylib 跨边界 | 全部 Rust |
| 扩展体系 | **运行时加载**：扩展/插件是 TS 模块，`import()` 进进程；另有 `hooks/` 旧子系统（用户可配置 shell 钩子） | **编译期静态注册**：`linkme` 分布式切片 + `Extension` trait（`src/core/extensions.rs`；`src/extensions.rs` 下 32 个扩展模块） |
| 工具注册 | 集中式 `BUILTIN_TOOL_NAMES`（`src/tools/builtin-names.ts`，30 个）+ 运行时工具工厂 + `xd://` 延迟工具 | 内置工具（`src/core/tools/`）+ 各扩展 `tools()`；延迟工具走 `ToolExposure::Deferred` + `tool-search` |
| 输入面 | 内部 URL scheme（`src/internal-urls/`，8529 行，`read`/`write` 统一路由） | `read`/`write` 只吃真实路径，无虚拟 URI |
| 运行形态 | 单进程 TUI；另有 `omp` RPC、collab 中继、ACP（编辑器驱动）、`omp stream` | 单进程 TUI；无服务端 |
| 遍历/搜索 | `pi-walker` + `pi-vfs` 进程内遍历；`ripgrep`/`glob`/`find` 内联 | `grep`/`find` **shell out 到外部 `rg`/`fd`**（`src/core/tools/grep.rs:100`、`find.rs:78`，`src/core/tools_manager.rs` 负责探测与下载） |
| Shell | `pi-shell` + `pi-builtins`：进程内 brush 分叉 + 71 个 utility builtin，零 fork/exec | 外部 shell（`src/core/tools/bash.rs` 直接 spawn） |

**结论**：两条路线的重心不同。oh-my-pi 的强项集中在「把宿主系统整个搬进进程」，
而这一块恰好是**最不可能迁移**的（§4.11、§4.12）；它的另一块强项
「把外部格式/外部工具接进工具面」是**最容易迁移**的，因为都是纯解析 + 纯 I/O（§3.1–§3.3）。

### 1.2 工具面差异

oh-my-pi 的 30 个内置工具（`packages/coding-agent/src/tools/builtin-names.ts`）：

```
read bash edit ast_grep ast_edit ask debug ida eval github glob grep find
lsp checkpoint rewind context_notes new_context security_scan task wait todo
web_search write memory_edit retain recall reflect learn manage_skill
```

（另有 3 个隐藏工具 `yield` / `goal` / `think`。README 自称「31 个内置工具」，差的那几个是
`browser` / `computer` / `generate_image` / `tts`——按代码它们**不是 AgentTool**：
`browser` 与 `computer` 是 eval prelude（`tools/browser.ts:251 createBrowserPrelude`），
`generate_image` / `tts` 是 SDK 注入的 `CustomTool`（`tools/image-gen.ts:236`、`tools/tts.ts:176`）。）

| prux 侧 | 覆盖的 oh-my-pi 工具 |
|---|---|
| 内置 `read` / `write` / `edit` / `bash` / `powershell` / `grep` / `find` / `ls` | `read` / `write` / `edit` / `bash` / `grep` / `find` / `glob`（`glob` 与 prux 的 `find` 冗余，prux 的 `find` 本身就是 fd 式 glob 查找） |
| `browser` | `browser`（omp 的 `browser` 其实是 eval prelude，不是 AgentTool） |
| `web-access`（`web_search` / `fetch_content` / `get_search_content`） | `web_search` |
| `mcp` | MCP 工具面 |
| `subagent` + `SubagentWorkflow` | `task` / `yield` |
| `tasks`（7 个 `Task*` 工具 + widget + 提醒注入） | `todo`（会话待办；omp 的 `todo` 多「阶段」与 Markdown 导入，见 §4.16） |
| `memory`（单工具 `memory`，BM25 召回） | `retain` / `recall` / `reflect` / `memory_edit`（omp 按后端拆四个） |
| `skills`（面板 + 内嵌技能 + settings 过滤） | `learn` / `manage_skill`（§3.6） / `skill://` |
| `rewind`（撤回最后一次用户输入，`src/extensions/rewind.rs`） | `rewind` / `checkpoint`（**语义不同**：omp 的 `rewind(report)` 是「折叠探索过程只留报告」，prux 没有对应物，见 §6.1） |
| `plan-mode` / `goal` / `proxy` / `usage` / `jet` / `loop-detect` / `hang-detect` / `notify` | `goal` / 本地网关 / `/usage` / `prewalk`(§4.16) / 循环与假死看门狗（另有 `AbortRun` / `RestartRun` UI 原语） |

prux **完全没有**的：`ast_grep`、`ast_edit`、`ask`、`debug`(DAP)、`ida`、`eval`、
`github`、`lsp`、`security_scan`、`wait`、`context_notes` / `new_context`、`glob`(冗余)。
本档的 §3 就是这条清单过筛后的结果（不在 §3 的，都能在 §4 找到否决理由）。

### 1.3 prux 扩展系统的能力边界（判断可行性的依据）

判断可行性全靠这张表。`Extension` trait 的完整面见 `src/core/extensions.rs:359` 起，
hook 面见 `src/core/extensions/hooks.rs`（`ExtensionHook` 13 项）。摘要：

**扩展能做**
- 注册模型可见工具（`tools()`，可声明 `Deferred` 延迟暴露）、命令（`commands()`，**框架每次查询都重新调用**，
  因此可以按工程动态返回）、快捷键、CLI flag（`cli_flags()`）；
- hook：`TransformContext`（自由改写消息列表）、`ContextWithSystem`（拥有提示词与消息）、
  `BeforeToolCall`（可 block + 改参数）、`AfterToolCall`（改结果）、`AgentEvent`、`Boundary`
  （`turn_end` / `agent_before_settle`，可 `continue` / `end` / 追加**投影草稿**：
  `with_omit` / `with_content_replace` / `with_retain_none_compaction`）、`ProviderStreamEvent`（只读）、
  `UserPrompt`（提交流改写）、`Suggestions`、`CacheWarmingDecision`、`ExtensionEvent`（扩展间总线）；
- UI：`request_ui`（Select 面板 / 通知 / 自定义卡片 / 覆盖层 / 停靠面板 / 设置面板）、
  工具执行期间由 TUI 主循环每帧处理一个请求（`src/modes/interactive.rs:339`、`:1080`）；
- 模型面：`virtual_models()`、`image_apis()` / `classifier_apis()`（provider 层已有
  `generate_images` / classify 的协议实现，见 `src/core/provider/images.rs`）、`custom_summarize()`；
- 工具集裁剪：`filter_tools()` / `filter_extension_tools()` / `prepare_loadout()`；
- TUI：`tool_renderers()` / `dock_lines()` / `overlay_view()` / `render_custom_message()`。

**扩展做不到**（本档据此把若干项判为「不做」）
- **流中打断并重驱动**：`ProviderStreamEvent` 只能缓冲观察，`Boundary` 只在轮末/结算前触发，
  `AbortRun` / `RestartRun` 是「中止当前回合」而不是「丢半成品后从同一处续跑」→ TTSR 做不了（§4.1）；
- **在 `before_tool_call` 里等用户答复**：该 hook 是同步的，UI 结果走 `on_ui_choice` → follow-up prompt，
  没有「回传给这个工具调用」的通道 → 交互式审批做不了（§4.4）；
  但**工具内部自己发 `request_ui` 并 await 一个自建通道**是可行的（§3.5，需先验证）；
- **按轮覆盖 thinking 档 / 改 provider 载荷 / 重置 provider session**：无对应 hook（`ultrathink` 因此打折，§3.11；§4.2）；
- **让 `read` / `write` 认虚拟路径**：内置工具不接受 URI（§4.5）；
- **运行时加载任意代码**：静态注册是设计前提（§4.6）。

---

## 二、prux 已覆盖（**不需要移植**）

| oh-my-pi 的功能 | prux 对应物 | 备注 |
|---|---|---|
| `task` 子代理扇出 + 类型化结果 + 工作树隔离 | `subagent` 扩展（`Agent` / `get_subagent_result` / `steer_subagent` + `SubagentWorkflow` + `/agents`） | omp 的 Agent Hub（`/hub` 实时名册）在 prux 是 FleetView / dock 段 |
| `todo` 会话待办 + 完成提醒 | `tasks`（`Task*` 七工具 + widget + 提醒注入）+ `plan-mode`（`Plan:` 步骤 + `[DONE:n]`） | 见 §4.16 |
| 技能（SKILL.md、发现、`/skills`） | `skills` 扩展 + `src/core/skills.rs` | prux 也已发现 `.agents/skills` 与 `~/.agents/skills`（`src/core/skills.rs:534`） |
| 记忆（`retain` / `recall` / `reflect`） | `memory` 扩展（BM25 词法召回 + 每 12 轮抽取一次） | omp 是多后端可插（§4.7） |
| `/btw` 只读旁问、`/recap` 小结 | `recap_btw` 扩展 | — |
| `/compact`、自动压缩、缓存保温 | core 压缩管线 + `cache-watch` / `cache-usage` | — |
| `web_search` 聚合检索 + 页面转 Markdown | `web-access`（8 个 provider） | omp 是 23 个 provider；数量差属 provider 适配，非扩展（§4.15） |
| `browser`（CDP）+ `/browser`、浏览器中继 | `browser` 扩展（CDP 直连、16 个动作） | prux 无「借用已开 Chrome」的中继（需浏览器插件，§4.3） |
| `read` 读 PDF / 图片 / notebook / SQLite | `pdf` 扩展（文本层）、内置 `read` 的图片附件 | 文档格式面见 §3.3 |
| `/export`（HTML）、`/share`、`/dump` | `/export` / `/share` / `bug`（诊断 zip） | — |
| `/model`、模型角色（`smol` / `slow` / `plan` / `vision` / `judge`） | `/model`、`/scoped-models`、`virtual-models` + `jet` 路由器 | 见 §4.15 |
| `/doctor`、`/usage`、`/changelog`、`/hotkeys`、`/theme`、`/settings`、`/resume`、`/fork`、`/tree`、`/label` | 同名内置命令 / `doctor` / `usage` / `extra-themes` | — |
| 循环检测（`loop-detect`）、假死看门狗、通知 | `loop-detect` / `hang-detect` / `notify` | — |
| MCP 客户端（`/mcp`、OAuth） | `mcp` 扩展 | — |
| 本地 OpenAI 兼容网关（`omp serve`） | `proxy` 扩展（入站网关） | — |
| 提示词模板 / 自定义命令 | `prompt-templates` + `agent_dir()/prompts` 与 `<cwd>/.prux/prompts` | — |

---

## 三、能够作为扩展的功能（15 项）

每节格式固定：**上游实现 → 机制 → prux 落点 → 工作量 / 风险 → 落地记录（落地后回填）**。

### 3.1 `ast-grep`：结构化搜索与结构化改写

**上游实现**
- 工具入口：`packages/coding-agent/src/tools/ast-grep.ts`（387 行）、`ast-edit.ts`（544 行）；
- 引擎：`crates/pi-natives/src/ast.rs`（1946 行）+ `crates/pi-ast/`（6145 行，
  `language/mod.rs` 的 `SupportLang::all_langs()` 是语言真源，57 种）；
- 模型侧提示词：`src/prompts/tools/ast-grep.md`；文档：`docs/tools/ast-grep.md`、`docs/tools/ast-edit.md`。

**机制**
- 模式语法：`$NAME` 捕获一个 AST 节点、`$_` 匹配不绑定、`$$$NAME` 捕获零或多个、`$$$` 不绑定；
  元变量必须大写且代表完整节点；同一元变量复用要求两处代码完全相同；`"key": $V`
  这类 JSON 成员片段有对象包裹回退；
- 输出按文件分组，匹配行以 `[PATH#HASH]`（hashline 模式）或 `*LINE|text` 呈现，
  多层匹配的续行带前导空格；`meta: NAME=value, …` 行报捕获值；
- `ast_edit` 是**预览 + 应用**两段式：`ast_edit` 只预览（`applied: false`，结果以
  `Staged as a proposal — files NOT modified yet...` 开头），真正的落盘要等代理对 `xd://resolve`
  做一次纯文本 `write`（body 是「为什么改」），由 `src/tools/resolve.ts` 的暂存队列回放重写；
  回放时会**重跑一遍**并比对替换数，对不上就报 `stalePreview`；期间还有一条
  `SoftToolRequirement(toolName: "write")` 的强制提醒；
- 工具默认关闭（`astGrep.enabled = false`），开启后是**可发现工具**（discoverable），
  冷门语言的 tree-sitter 语法按需从 GitHub release 下 `.wasm`（`src/utils/grammars.ts`、
  `crates/pi-natives/src/grammars.rs`）。

**prux 落点**
- 新扩展 `src/extensions/ast_grep.rs`（模块内自声明工厂静态，priority 与 `browser` 同档）；
- crate 侧：`ast-grep-core`（crates.io，MIT，0.51.0，可选 `tree-sitter` feature）+ 需要的
  `tree-sitter-*` 语法 crate。prux 目前**没有任何 tree-sitter 依赖**（`Cargo.toml` 已核），
  这是本项唯一的成本大头；
- 工具用 `ToolExposure::Deferred` 暴露（prux 有 `tool-search` 扩展，正是为这种大工具准备的），
  避免常驻占一个工具位；
- 遍历部分不必引 `pi-walker`：prux 已依赖 `ignore`，扩展内用 `ignore::WalkBuilder` 即可；
- 「预览 + 应用」在 prux 里**不能照抄**：omp 的应用入口是一次「对虚拟路径的 `write`」，
  prux 的 `write` 只吃真实路径（§4.5）。可用的等价形态有两种：
  1. **两次调用**：`ast_edit` 先只预览并返回一个 `proposalId`，第二次调用带 `proposalId` 才落盘
     （扩展内自持暂存表 + 过期校验，对齐 `stalePreview` 的语义）；
  2. **`BeforeToolCall` 软要求**：预览后在下一轮用 `AfterToolCall` / 边界追加一条
     「必须调用 `ast_edit(... apply: true)`」的提醒（prux 无 `SoftToolRequirement` 原语，
     只能用提醒文本近似）。

**工作量 / 风险**
- 中等偏大（一次引入 tree-sitter 生态）。风险点：语法 crate 的体积与编译时间、
  冷门语言是否要做按需下载（建议**不做**，直接砍到内置语法集，缺失语言明确报错）。

**落地记录**：（待落地）

### 3.2 `agent-import`：八种外部 agent 配置导入

**上游实现**
- `packages/coding-agent/src/discovery/` 下的解析器（行数为 `wc -l` 实测）：
  `claude.ts` 648、`codex.ts` 634、`gemini.ts` 373、`github.ts` 340、`agents.ts` 330、
  `cursor.ts` 223、`windsurf.ts` 152、`vscode.ts` 106、`cline.ts` 84、
  `agents-md.ts` 30、`claude-md.ts` 30；
- 统一入口 `discovery/helpers.ts:269 discoverRuleFromMarkdown` / `:200 buildRule`；
- 文档：`docs/context-files.md`、`docs/config-usage.md`；README §15。

**机制**：八处只读扫描，归一化成 context-file / rule / instruction / prompt / skill / MCP / hooks：

| # | 来源 | 读取路径 | 格式要点 |
|---|---|---|---|
| 1 | Claude Code | `~/.claude/CLAUDE.md`、`<cwd>/.claude/{CLAUDE.md,commands,skills,rules}` | Markdown（frontmatter 可选） |
| 2 | Cursor | `~/.cursor/rules/*.{mdc,md}` | MDC frontmatter：`description` / `globs` / `alwaysApply`（`cursor.ts:139` `transformMDCRule`） |
| 3 | Windsurf | `~/.codeium/windsurf/memories/global_rules.md`、`.windsurf/rules/*.md` | Markdown |
| 4 | Gemini CLI | `~/.gemini/GEMINI.md`、`.gemini/settings.json` | Markdown + JSON |
| 5 | Codex | `~/.codex/AGENTS.md`（仅用户级） | Markdown |
| 6 | Cline | 最近祖先的 `.clinerules`（文件或目录） | 单文件 / 目录 Markdown（`cline.ts:47-75`） |
| 7 | GitHub Copilot | `.github/copilot-instructions.md`、`.github/instructions/**/*.instructions.md`、`.github/prompts/*.prompt.md`、`.github/skills/*/SKILL.md` | `applyTo:` frontmatter（逗号分隔 glob，`github.ts:188-224` 归一化：`*`/`**`/`**/*` → alwaysApply） |
| 8 | VS Code | `.vscode/mcp.json`（嵌套 `mcp.servers`） | JSON |
| 附 | `.agent` / `.agents` | `~/.agent[s]/`、祖先 `.agent[s]/` | rules / skills / prompts（**prux 已部分覆盖**：`.agents/skills` 已扫描） |

**prux 落点**
- 新扩展 `src/extensions/agent_import.rs`。产出三类落点，都不必改 core：
  1. **上下文文件** → 由 `ContextWithSystem` hook 把扫描到的规则正文追加进本次请求
     （或经 `custom_prompt` 类路径并入系统提示；core 侧 `context_files` 只认
     `src/core/context.rs:10` 的 5 个候选名，扩展不要动它）；
  2. **规则 → 技能/模板**：prux 已经发现 `<cwd>/.prux/skills`、`.agents/skills`、
     `agent_dir()/skills`（`src/core/skills.rs:534`）与 `agent_dir()/prompts`、`<cwd>/.prux/prompts`
     （`src/core/prompt_templates.rs:339`）。扩展的职责是**把外部格式物化到这些目录**（可逆、
     幂等、不覆盖用户文件），而不是再造一套发现逻辑；
  3. **Copilot `*.prompt.md` / Claude `.claude/commands`** → `commands()`（框架每次查询都重新调用，
     可按工程动态返回）+ 提示词模板。
- 开关放 `settings.json`（`enabledProviders` 式的逐来源 opt-in），因为**读取项目内的
  `.cursor` / `.github` 需要项目信任**（prux 有 `project_trust`，扫描前要过这道门）。

**工作量 / 风险**
- 小偏中，纯文件解析 + 幂等物化，无外部依赖。风险点：`globs` / `applyTo` 的**条件注入**
  （「命中这些 glob 才生效」）在 prux 没有对应层，需要扩展自建匹配并在 hook 里按当前
  待处理文件判定；建议先做「无条件注入」版本，条件注入列为后续切片。

**落地记录**：（待落地）

### 3.3 `markit`：文档 → Markdown（并把 `pdf` 扩展并进来）

**上游实现**
- `packages/coding-agent/src/markit/`（821 行）：`registry.ts` 是入口，`converters/` 下
  `pdf/index.ts`、`docx.ts`、`pptx.ts`、`xlsx.ts`、`epub.ts`；`NOTICE` 注明改编自 markit-ai（MIT）；
- docx 走 `mammoth` → HTML → turndown（`docx.ts:31-40`），把内嵌图片抽到 `imageDir`；
- 文档：`docs/tools/read.md`。

**机制**：`Markit.convert()` 遍历注册的 converter，第一个 `accepts()` 命中且 `convert()` 成功的胜出；
全部失败时报 `Conversion failed: <每个 converter 的错误>`，无匹配时 `Unsupported format: <ext>`。

**prux 落点**
- prux 已有 `pdf` 扩展（`read_pdf` 工具，`pdf-extract` 0.12，只抽文本层）。
  建议**扩成 `markit`**：`read_doc` / `read_pdf` 同工具按扩展名分派，或保留 `read_pdf`
  再加 `read_doc`。Rust 侧选型（均已核在 crates.io 存在）：
  - `.xlsx` / `.xls` / `.ods` → `calamine`（MIT，纯 Rust，0.37.0）；
  - `.docx` → `docx-rs` / `docx-rust`（MIT；注意前者偏「写」，需要确认读取 API），
    或 `zip` + `quick-xml` 直接抽 `word/document.xml` 的文本；
  - `.pptx` → `zip` + `quick-xml`（无成熟 crate，自己遍历 `ppt/slides/slideN.xml`）；
  - `.epub` → `zip` + HTML → **prux 已依赖 `html2text` 0.17**，可直接复用；
- 截断口径沿用 `read`：2000 行 / 50KB；超过 32 MiB 在解析前拒绝（`pdf` 扩展已有此策略）；
- 扫描件（图片型 PDF）与无文本层的情况要**明确提示**而不是返回空，这条 `pdf` 扩展已做对。

**工作量 / 风险**
- 中等。风险点：docx / pptx 的 Rust 生态不如 JS 成熟，版面还原度会**明显低于 mammoth + turndown**；
  参考实现（用 MarkItDown / pandoc 之类外部命令）是一条退路，但会引入外部二进制依赖
  （prux 的 `downloader` 扩展能装 `fd`/`rg`，再加一个「按需下载 pandoc」是可行但要谨慎的口子）。

**落地记录**：（待落地）

### 3.4 `shake`：手动丢弃重内容

**上游实现**
- 命令声明：`src/slash-commands/builtin-lifecycle.ts:277-284`（`elide` / `images` / `thinking` 三个子命令）；
- 处理：`src/modes/controllers/command-controller.ts:2012 handleShakeCommand` → `session.shake(mode)`；
- 摘要格式化 `formatShakeSummary`，聊天区重建 + `clearScrollback`。

**机制**：三档一次性丢内容——`elide` 丢工具结果与大块、`images` 丢图片块、`thinking` 丢全部
thinking 块；丢完重建聊天区并报告各项条数（`Nothing to shake.` 表示无事可做）。

**prux 落点**
- 新扩展 `src/extensions/shake.rs`：`/shake [elide|images|thinking] [all]` 只是个**开关/一次性标记**，
  真正的丢弃放在 `transform_context` hook 里（该 hook 的签名是 `&mut Vec<AgentMessage>`，
  可以自由改写消息列表；丢弃是**请求侧投影**，不改会话文件，因此历史与 `/tree` 都不受影响）；
- 与既有能力的关系：prux 的 `Boundary` 投影草稿（`with_omit` / `with_content_replace`）走的是
  **落库**路径（`src/core/extensions.rs:1027`/`1036`），语义比 `shake` 重。`shake` 用
  `transform_context` 更贴合「只在下次请求里少一点」。**两者不要混用同一实现**；
- 统计口径（丢了几条工具结果 / 几张图 / 几段 thinking）要在命令执行后作为通知回给用户，
  否则用户不知道有没有生效。

**工作量 / 风险**
- 小。风险点：只能丢**当前可见消息列表**里的内容；若被自动压缩过，可丢的东西本来就不多，
  要给出诚实的「无事可做」。另外 `transform_context` 每次请求都跑，标记必须是**一次性**的，
  否则会一直丢。

**落地记录**：（待落地）

### 3.5 `ask`：结构化提问工具

**上游实现**
- `packages/coding-agent/src/tools/ask.ts`（1026 行）+ `docs/tools/ask.md`；
- UI 侧：`packages/tui/src/tools/ask.ts`（调用/结果归一化与渲染）；设置 `ask.enabled` / `ask.timeout` / `ask.notify`。

**机制**
- 入参是一个 `questions[]`：每个问题有 `id`（同一调用内唯一）、`question`、`options[]`
  （`label` 在问题内唯一，可选 `description` / `preview`）、可选 `header`、
  `multi`（多选）、`recommended`（默认项下标，越界则不给推荐）；
- 输出：单问题时给「选定答案 + 可选 `User added note:`」；多问题时 `User answers:` 后按 `id` 逐行；
  另有「用户选择改在聊天里讨论」的 `chatRedirect` 分支；
- 取消与无头模式**抛错**而不是返回结构化成功结果；plan mode 下禁用超时。

**prux 落点**
- 新扩展 `src/extensions/ask.rs`，工具名 `ask`（或 `AskUserQuestion`）；
- 通道：工具内 `request_ui(ExtensionUiRequest::Select { id, title, options })`，然后
  **await 一个扩展自持的 oneshot/通道**，由 `on_ui_choice(id, choice)` 兑现。
  prux 已有同类桥接先例（`src/extensions/tasks/subagent.rs` 用 `oneshot` 把「发请求 → 等回执事件」
  桥成一次 `await`），机制上成立；
- 多选与自由输入：prux 的 `Select` 面板只有单选回传串，多选需要扩展自己编码
  （例如用 `SelectOption.text` 携带 `<id>:<option>`，多次调用来回），或走 `overlay_view()` 自绘面板；
- 超时：用 `ask.timeout` 同款设置键（放扩展的设置面板里）。

**工作量 / 风险**
- 中等。**必须先验证一件事**：工具执行期间 TUI 仍在取 UI 请求——`src/modes/interactive.rs:339`
  的主循环里 `maybe_handle_extension_ui` 每帧调用，看起来成立，但没有现成扩展走这条路径，
  值得先写一个最小验证（一个工具发 Select、await、断言能拿到回传）。

**落地记录**：（待落地）

### 3.6 `manage_skill`：会话内沉淀技能（并入 `skills` 扩展）

**上游实现**
- `packages/coding-agent/src/tools/manage-skill.ts`（104 行）+ `src/autolearn/managed-skills.ts`；
- 文档：`docs/tools/manage_skill.md`；门控 `autolearn.enabled=true`（默认 false）。

**机制**（`manage-skill.md` 逐条核过）：
- 三个动作 `create` / `update` / `delete`；名字 trim + 小写化后必须匹配 `[a-z0-9][a-z0-9-]{0,63}`；
- 落盘到 `<agent-dir>/managed-skills/<name>/SKILL.md`，最终内容上限 64 000 字节；
- `create` 用**独占**语义（`wx`），已存在则失败；**被 authored 技能占用时直接拒绝**
  （返回 `isError` + `details.shadowed = true`），保证「手写技能永远压过自动沉淀的技能」；
- 变更后刷新活跃技能列表；同名的进程内写入串行、跨进程不串行。

**prux 落点**
- **不新起扩展**：并进 `skills` 扩展（`src/extensions/skills.rs`），加一个 `manage_skill` 工具
  与一个 `/skill create|update|delete` 命令；
- 与 prux 现有机制对齐：prux 的 skills 扩展已有「草稿 → `Ctrl+S` 写 `settings.json` 的 skills 过滤数组
  → 重建 skill 工具」的落盘路径（`assets/docs/extensions.md` 的 `skills` 条目），
  自动沉淀应当写到一个**受管目录**（如 `agent_dir()/skills-managed/`），并在发现顺序里
  **排在用户手写技能之后**（prux 的先到先得顺序见 `src/core/skills.rs:534`）——
  这条「让位」规则必须显式实现，否则模型会用自己的技能盖掉用户的。

**工作量 / 风险**
- 小。风险点：技能数量与质量需要护栏（上限 64KB 同款 + 名字校验 + 让位规则），
  否则模型会沉淀出几十个低质技能污染候选列表。

**落地记录**：（待落地）

### 3.7 `handoff`：交接文档 + 就地压缩

**上游实现**
- 命令：`src/slash-commands/builtin-lifecycle.ts:306-308`（`/handoff [focus instructions]`）；
- 管线（`docs/handoff-generation-pipeline.md` 有完整时序）：`CommandController.handleHandoffCommand`
  → `AgentSession.handoff()` → `SessionHandoff.generateDocument()`（`src/session/session-handoff.ts`，243 行）
  → 作为 `CompactionEntry`（`method: "handoff"`）**就地提交**；
- 前置校验：流式中或压缩中拒绝；`type === "message"` 条目 < 2 时提示 `Nothing to hand off (no messages yet)`。

**机制**：一次独立请求把当前会话压成一份「交接文档」，然后**立刻**作为压缩条目落库——
session id / 文件 / cache key 都不变，近端历史保留，远端被文档替换。

**prux 落点**
- 新扩展 `src/extensions/handoff.rs`，`/handoff [focus]` 命令 + 一次独立请求；
- 落库用现成接口：`Extension::custom_summarize`（`src/core/extensions.rs:516`）或
  `on_before_compact`，再把结果作为压缩条目写回；prux 的压缩管线已有 `custom_summarize` 接缝，
  不需要新增 core 能力；
- 与 `recap` 扩展的区别要写清：`recap` 只把小结**推到聊天区**（不改上下文），
  `handoff` 是**替换上下文**。两者别合并。

**工作量 / 风险**
- 小。风险点：交接文档的质量完全取决于模型，prux 需要给出固定的文档骨架
  （目标 / 已完成 / 未完成 / 关键文件 / 下一步），否则压缩后信息损失不可控。

**落地记录**：（待落地）

### 3.8 `generate_image`：图像生成工具

**上游实现**
- `packages/coding-agent/src/tools/image-gen.ts`（361 行）+ `docs/tools/generate_image.md`；
  传输层在 `packages/ai/src/images/`；门控 `generate_image.enabled=true`（默认 false）。

**机制**：入参把提示词拆成 `subject`（必需）/ `action` / `scene` / `composition` / `lighting` /
`style` / `text` / `changes[]`（编辑指令）/ `aspect_ratio` / `image_size` / `input[]`（编辑输入图，
按路径或内联 base64）/ `model`（图像模型选择器）；输出把图片写到临时路径，`details` 带
provider / model / `imagePaths` / `images`（含 base64 与 mimeType）。协议主要是
OpenAI 兼容的 `/images/generations` 与 `/images/edits`（后者 OpenAI 端是 multipart）。
注意它在 omp 里**不是内置工具**，而是 SDK 注入的 `CustomTool`
（`tools/image-gen.ts:236`，由 `getImageGenTools()` 提供），门控 `generate_image.enabled`
（默认 `false`）。

**prux 落点**
- 新扩展 `src/extensions/gen_image.rs`，工具 `generate_image`；
- **prux 的 provider 层已经有一半**：`src/core/provider/images.rs` 实现了
  OpenAI 兼容图像生成（`generate_images`，`modalities` + `data:` URL 解析），
  且 `Extension::image_apis()` 允许扩展贡献新的图像协议实现；
- 第二半也在：**TUI 能显示图片**（`src/modes/interactive/render/image.rs` 走 `ratatui-image`，
  kitty / sixel / iTerm 协议），所以生成结果可以直接内联展示，而不是只给一个路径；
- 现状缺口：prux 只有 `codemode` 的 `models.generateImages`（且 `codemode` 默认关闭）。
  本项就是把这条能力**提升为常驻工具**。

**工作量 / 风险**
- 小偏中（工具壳 + 参数校验 + 落盘 + 展示）。风险点：图像模型的选择与凭据
  （prux 的 `image_apis` 是协议面，「哪个 provider 的哪个模型能出图」需要模型目录里有
  带 image 输出的条目）；建议第一版只支持「当前 provider 的 OpenAI 兼容图像端点」，
  不引 omp 那套 `image` 角色链。

**落地记录**：（待落地）

### 3.9 `advisor`：第二模型旁观每轮

**上游实现**
- `src/advisor/runtime.ts`（1888 行）、`advise-tool.ts`（362 行）、`emission-guard.ts`（323 行）、
  `watchdog.ts`（148 行）；
- 命令：`src/slash-commands/builtin-collaboration.ts:63-73`（`/advisor on|off|status|dump|configure`）；
- 文档：`docs/advisor-watchdog.md`。

**机制**：把一个（或多个）reviewer 模型挂到会话上，**按主代理的增量**审阅，有自己的 ToolSession
与自己的上下文；产出 `advise` 工具调用，带 `nit` / `concern` / `blocker` 三档严重度，
注入到主上下文里呈现为 `<advisory>` 卡片。`emission-guard` 负责去重、每轮配额、
以及过滤「说了等于没说」的短语；压缩或切换会话时重置。

**prux 落点**
- 新扩展 `src/extensions/advisor.rs`：`/advisor on|off|status` 命令 + 设置面板（选模型、档位、配额）；
- 实现路径：扩展自己起一个**独立 agent 回合**（不经主 agent 的会话；`subagent` 扩展已证明
  扩展可以自己跑模型请求），把主会话最近增量（`AgentEvent` 里能拿到消息与工具调用）喂给它，
  产出经 `Boundary`（`turn_end` 的 `continue_with(append)`）或 `TransformContext` 注入主上下文；
- 「自己的上下文」用扩展自持的状态 + 落盘（对齐 `memory` 扩展的做法），不要写进主会话，
  否则主会话的 cache 命中率会被 reviewer 的流水账毁掉。

**工作量 / 风险**
- 中等（最大的两个坑：注入时机与去重）。风险点：注入内容会**改变主会话的 prompt 前缀**，
  每轮注入 = 每轮 cache miss；omp 有 `emission-guard` 做配额正是为此，prux 侧要等效实现
  （建议只在 `blocker` 档注入，`nit` / `concern` 进 dock 面板而不是进上下文）。

**落地记录**：（待落地）

### 3.10 `cleanse`：检查器驱动的批量修复

**上游实现**
- `packages/coding-agent/src/cleanse/`（3449 行）：`agent.ts`、`checkers.ts`、`loop.ts`、
  `parsers.ts`、`balance.ts`、`types.ts` + `prompts/{assignment,discovery,follow-up}.md`；
- 命令：`src/slash-commands/builtin-lifecycle.ts:477-479`
  （"Detect and fix project diagnostics with weighted parallel subagents"）；
  默认模型 `@smol`。

**机制**：先**发现**项目里的 checker（lint / typecheck / shellcheck 等，`checkers.ts` 里有
`IGNORED_DIRECTORIES` 与 `SHELLCHECK_BATCH_SIZE` 之类的批处理参数），跑一遍收集诊断，
按文件分组并按权重平衡到多个子代理，每个子代理拿一份「作业单」（`assignment.md`）去修，
最后汇总。UI 有 board（`createCleanseStatusBoard`）与 picker。

**prux 落点**
- 新扩展 `src/extensions/cleanse.rs`，`/cleanse` 命令；
- 子代理派发走 `tasks` 扩展已建立的 RPC 路径（`tasks` 的 `TaskExecute` 经 RPC 联动 `subagent`，
  见 `assets/docs/extensions.md` 的 `tasks` 条目），因此本项**依赖 `subagent` + `tasks` 开启**；
- 进度展示用 `dock_lines()` + `request_show_dock()`（`plan_mode` 的 `/todos` 是同款模式）。

**工作量 / 风险**
- 中等偏大。风险点：checker 发现与诊断解析高度**工程相关**（每种 lint 的输出格式都要解析）；
  建议第一版砍到「用户显式给出命令」而不是自动发现，例如 `/cleanse cargo clippy`，
  自动发现列为后续切片。

**落地记录**：（待落地）

### 3.11 `magic-keywords`：`ultrathink` / `orchestrate` / `workflowz`

**上游实现**
- 匹配与声明：`packages/coding-agent/src/modes/magic-keywords.ts`（113 行）；
- TUI 侧高亮/动画：`packages/tui/src/prompt/magic-keywords.ts`（154 行）；
- 文档：`docs/magic-keywords.md`；设置 `magicKeywords.*`。

**机制**
- 必须**精确小写、独立成词**；句子标点/引号可以贴边，但字母 / 数字 / `_` / `/` / `\` / `-` /
  扩展名 / `::` / 调用语法都不算；忽略围栏代码块、行内 code、HTML/XML 注释与标签；
- 用 **slash 展开后**的 prompt 匹配；合成 / 代理自发的 prompt 不触发；只作用于该轮；
- 注入：每个关键词向该轮注入一条**隐藏的、归属 user 的 notice**（可见词仍留在 user 消息里）；
- 三者语义：`ultrathink` 要求多步推理并**把该轮 thinking 拉到最高档**；
  `orchestrate` 要求「用并行子代理做相互独立的工作并逐阶段验证」（需 `task` 启用）；
  `workflowz` 要求「用 `task` 造确定性工作流」（需 `eval` + `task` 启用）。

**prux 落点**
- 新扩展 `src/extensions/magic_keywords.rs`，纯 hook 实现：
  - 匹配在 `on_user_prompt` 或 `on_user_submit`；注入用 `UserPromptAction`（改写/追加隐藏消息）；
  - `orchestrate` / `workflowz` 的目标在 prux 都存在（`subagent` + `SubagentWorkflow`），
    注入文本里直接点名它们即可，且应在两个扩展未启用时不触发；
- **`ultrathink` 打折扣**：prux 没有「按轮覆盖 thinking 档」的扩展接口
  （`/thinking` 是内置命令、`Extension` trait 里没有 thinking 相关动作）。
  两种选择：(a) 只做提示词层面的「要求仔细推理」，不承诺档位；
  (b) 先给 core 补一个 `ExtensionUiRequest::SetThinking { level }` 之类的单轮动作，再做完整版；
- 「排除代码块 / 行内 code」这套匹配规则要原样实现（否则 `grep ultrathink` 这种查询会误触发），
  这是一段独立的、值得配单测的解析逻辑。

**工作量 / 风险**
- 小（不含 `ultrathink` 的 core 改动）。风险点：**注入 = 改用户消息**，
  要确保可见词不被动过（否则用户看到的与发出去的文本不一致），且注入内容不打乱 cache 前缀的稳定性。

**落地记录**：（待落地）

### 3.12 `loop`：同一提示词反复重投（直到闸门放行）

**上游实现**
- 命令：`src/slash-commands/builtin-modes.ts:416-425`；
- 语义（引自其 `description`）：开启后**你下一条提示词会在每次 yield 之后重新提交**；
  可以用「次数 / 时长」封顶，或用 `--until '<cmd>'` / `--while '<cmd>'` 按**子命令退出码**
  决定是否继续下一轮；`Esc` 挂起当前循环，再敲一次 `/loop` 关闭。
- 配套：`extensibility/custom-commands/bundled/ci-green` 是一份用它做「CI 变绿为止」的**打包命令**。

**机制**：这不是「让模型自己循环」，而是**宿主层重复投递同一条 user 消息**——
每轮结束（yield）时决定「再投一次 / 停下」，闸门是外部命令的退出码。

**prux 落点**
- 新扩展 `src/extensions/loop.rs`：`/loop [<次数>|<时长>] [--until '<cmd>'] [--while '<cmd>']` 命令 +
  **只用 `Boundary` 钩子**实现——`turn_end` 时按闸门决定 `BoundaryOutcome::continue_with(...)`
  （`src/core/extensions.rs:1010`）还是 `end()`；
- 闸门执行用 prux 现成能力（扩展内 spawn shell，或走 `execute_tool` 的 bash 路径）；
- 与 `goal` 扩展的分工要写清：`goal` 是「**模型判断**目标有没有完成」的自主循环，
  `loop` 是「**宿主判断**」的确定性重复投递；两者互斥启用，避免同一条消息被两条路径重复投递。

**工作量 / 风险**
- 小（prux 的 `Boundary` 原语正好是为这类「扩展要求续跑」设计的，`hang-detect` 的
  `RestartRun` 是同类先例）。风险点：无限循环保护——次数 / 时长上限**必须有默认值**，
  且闸门命令要有超时，否则 `/loop` 会变成一个停不下来的烧钱开关。

**落地记录**：（待落地）

### 3.13 `github`：gh CLI 封装（仅记录）

**上游实现**：`src/tools/gh.ts`（293 行）+ `gh-view` / `gh-search` / `gh-pr-checkout` /
`gh-run-watch` / `github-cache`（合计 4461 行）；文档 `docs/tools/github.md`；
依赖系统里的 `gh` CLI（`src/utils/github.ts` 包装）+ SQLite 缓存。

**机制**：仓库 / PR / issue 查看、代码搜索、Actions run-watch，全部经 `gh` 子命令；
上游还把它接到内部 scheme（`pr://` / `issue://`），但**那一半不算本项**（§4.5）。

**prux 落点**：新扩展 `src/extensions/github.rs`，`github` 工具（**延迟工具**，走 `tool_search`），
实现方式是扩展内 spawn `gh` 并解析 JSON 输出——prux 的 shell 能力足够，不需要 HTTP 直连 GitHub。
缓存可省（prux 无 SQLite 依赖，第一版不加）。

**工作量 / 风险**：小偏中。风险点：`gh` 不存在时要给明确安装提示
（prux 的 `tools_manager::install_hint` 已有同款文案模式）。**仅记录**：价值取决于用户是否常用 `gh`，
且「让模型直接调 `bash` 跑 `gh`」本来就可行，本项的收益是**稳定的参数 schema 与输出裁剪**。

**落地记录**：（待落地）

### 3.14 `python`：持久 Python cell（仅记录，并入 `codemode`）

**上游实现**：`src/tools/eval.ts`（1071 行）+ `src/eval/`（14948 行，JS 与 Python 两套 kernel）；
文档 `docs/tools/eval.md`、`docs/python-repl.md`。

**机制**：持久 cell——变量与导入在多次调用间保持；host bridge 允许 cell 里的代码
**回调 agent 自己的工具**（read / search / task），还能起子代理。

**prux 落点**：prux 的 `codemode` 是 **QuickJS 沙箱、每次调用新建运行时**
（`src/extensions/codemode/sandbox.rs:231` 建 `AsyncRuntime`，脚本结束即弃），
因此**没有持久性**，这是与 omp `eval` 的核心差距。若要补：在扩展内 spawn 一个 CPython 子进程，
用「行分隔 JSON + 哨兵」协议做请求/响应，进程常驻到会话结束。
JS 侧不必重做（QuickJS 已够），Python 侧才是增量。

**工作量 / 风险**：中等偏大。风险点：Python 解释器是否存在于目标机器（Windows 上尤其不确定）、
进程泄漏与超时、以及 cell 里回调工具需要一条 `ScriptHost` 式的桥。
**仅记录**：性价比取决于用户是否真有数据分析场景。

**落地记录**：（待落地）

### 3.15 `lsp`：只读查询半套（仅记录）

**上游实现**：`src/lsp/`（9947 行）：`tool.ts`（1575）、`client.ts`（1941）、`config.ts`（538）；
`src/lsp/defaults.json` 预置 rust-analyzer / clangd / typescript-language-server 等；
文档 `docs/lsp-config.md`、`docs/tools/lsp.md`；对外 14 个 op。

**机制**：扩展 spawn 语言服务器（stdio JSON-RPC），暴露诊断 / 跳转 / 符号 / 重命名 /
code action / 原始请求。**关键**：omp 的 LSP 与 `edit` / `write` 深度耦合——
写入后自动回灌诊断（writethrough）、延迟诊断队列、diagnostics ledger，
这些需要 core 配合，因此「完整版」不是扩展能做的。

**prux 落点**：新扩展 `src/extensions/lsp.rs`，只做**按需询问**：
`diagnostics` / `definition` / `references` / `symbols` / `hover` / `rename`（重命名只算「计算」，
真正改文件仍由 `edit` 执行）。扩展内自己实现 LSP 客户端（`tokio::process` + 行分隔 JSON-RPC），
不需要 core 改动。「写后自动回灌诊断」**不做**（属 core 与工具管线的耦合）。

**工作量 / 风险**：大（客户端 + 服务器发现 + 语言映射 + 生命周期管理）。
**仅记录**：这是本档里价值最高但也最贵的一项；如果要做，建议**先只做 `diagnostics`**
（收益最直接：写完代码立刻知道编译错），其余 op 后续再加。

**落地记录**：（待落地）

---

## 四、明确不实现（**均不做**）

排序：先「机制/策略」，再「已有扩展的辅助项」，再「扩展层够不到」。

### 4.1 TTSR：流中打断并重驱动（time-traveling stream rules）

- 上游：`src/session/ttsr-coordinator.ts`（733）、`ttsr-outputs.ts`（226）、
  `src/export/ttsr.ts`（1148）、`ttsr-settings.ts`（144）；文档 `docs/ttsr-injection-lifecycle.md`、`docs/rulebook-matching-pipeline.md`；
  相关命令 `/omfg`（从一句抱怨里铸一条 TTSR 规则，`builtin-lifecycle.ts:465`）。
- 机制：`turn_start` 重置缓冲 → 监控 `text_delta` / `thinking_delta` / `toolcall_delta` 与
  `toolcall_end` → 正则命中就**立即 `agent.abort()`** → 50ms 后延迟重试 → 以
  `contextMode: "discard"` 丢掉半成品消息 → 注入隐藏 `custom_message(customType:"ttsr-injection")`
  → 调度 `agent.continue()`；非打断式命中走 `afterToolCall` 的 `additionalContext`。
- 判「不做」的理由：**prux 没有这条原语**。`ProviderStreamEvent` 只能只读缓冲（归一化前）、
  `AgentEvent` 里的 `message_update` 只能观察、`Boundary` 只在轮末/结算前触发，
  `AbortRun` / `RestartRun` 是「丢弃当前回合并重开」而不是「从被打断处续跑」。
  要给完整 TTSR 必须先在 core 里加「流式拦截 → 中止 → 丢弃半成品 → 重注入 → 续跑」的钩子，
  这是**机制级**改动，不属扩展层。
- 退路（可作为 §3 之外的独立小项评估）：只做**非打断式**的近似——
  `AfterToolCall` 附加提醒 + `Boundary` 追加消息，即在轮末贴规则而不是中途打断。
  效果差很多（模型已经写完了），本档不推荐。

### 4.2 `/fresh`：重置 provider 流状态

- 上游：`builtin-lifecycle.ts:169-171` + `AgentSession.freshSession()`：
  关掉所有缓存的 provider session 句柄、铸新 provider session id、重设 memory key、
  失效 append-only 上下文；**不动 transcript / 会话文件 / header**。
- 判「不做」：prux 没有「provider session id」这一层抽象，扩展也没有重置 provider 状态的接口。
  这属 core（provider 层）能力。

### 4.3 `/collab` + `omp stream` + ACP + `/live` + `/record` + 浏览器中继

- `/collab`（`src/collab/`：`host.ts` 1262、`guest.ts` 965 + protocol / crypto / relay client）：
  把运行中会话实时复制给别的 omp 或浏览器（AES-256-GCM 客户端加密 + 中继服务器）；
  `omp stream`（`docs/stream.md`）把渲染后的屏幕行单向广播；
  ACP（`packages/coding-agent/src/acp/`）让编辑器驱动 agent；`/live` 是 Codex 实时语音；
  `/record` 录屏成可回放文件；浏览器中继需要浏览器端插件（`packages/browser-relay/`）。
- 判「不做」：全部需要**服务端 / 中继 / 屏幕行钩子 / 浏览器扩展**。沿用
  `migration-jcode-v0.94.0.md` §4.6–§4.8、§4.13 的同一结论（单 server 多 client、
  远程客户端、对外接口一律不做）。

### 4.4 交互式审批模式（approval mode）

- 上游：`docs/approval-mode.md` + `src/tools/approval.ts`：三级 tier（`read` / `write` / `exec`）+
  `tools.approval.<tool>` 的 `allow` / `deny` / `prompt` + CLI `--approval-mode` / `--auto-approve` / `--yolo`；
  bash 对 `rm -rf /` 这类命令强制 `override:true` 弹窗。
- 判「不做」（交互半套）：prux 的 `before_tool_call` 是**同步**的，只能 block + reason，
  无法在其中 await 一个 UI 面板；UI 结果的回传通道是 `on_ui_choice` → follow-up prompt，
  不是「回给这次工具调用」。
- **可做的静态半套**：用 `BeforeToolCall` 实现「按工具名 / 参数正则 allow / deny」
  （相当于 omp 的静态 `tools.approval`），这是 policy 而不是功能，按判断口径**不新起扩展**；
  prux 已有 `--exclude-tools` / `--no-tools` / `--no-builtin-tools`（`src/cli/args.rs:114-126`），
  omp 的 `--tools` 白名单因此**已被覆盖**，不需要移植。

### 4.5 内部 URL scheme 体系（`pr://` / `issue://` / `agent://` / `skill://` / `ssh://` / `local://` / `conflict://` / `xd://` …）

- 上游：`src/internal-urls/`（8529 行），核心是 `router.ts` 的 `InternalUrlRouter`：
  每个 scheme 是一个 `ProtocolHandler` + `SchemeSpec`（`backing`：file/virtual/remote；
  `write`：via/scope/tier；`selectors` / `immutable` / `linkable` / `readTier`），
  工具不再按 scheme 名分支，而是统一调 `target()` / `writeTarget()` / `writeTier()` / `describe(host)`。
  单文件规模：`issue-pr-protocol.ts` 619、`local-protocol.ts` 538、`agent-protocol.ts` 428、
  `ssh-protocol.ts` 365、`skill-protocol.ts` 230、`xd-protocol.ts` 136、`conflict-protocol.ts` 54，
  另有 `memory://` 487、`history://` 554、`cfg://` 473、`vault://` 852、`rule://` 282 等。
- 判「不做」：**这套东西的价值全部来自「`read` / `write` 认 URI」**，
  而 prux 的 `read` / `write` 是 Rust 内置、只吃真实路径。要移植必须**改 core 的两个内置工具**，
  属机制级改动。若只在扩展里新起一个「吃 URI 的工具」，就得不到「一套路径语义贯穿所有工具」的收益，
  反而多一套并行的路径语义——不做。
- 其中 `conflict://`（把每个 merge 冲突变成一个 URL，写 `@theirs` / `@ours` / `@base` 解决）
  是**有独立价值**的一项：prux 侧可以用一个**普通工具**（不引 scheme）实现，
  即 `resolve_conflict`：读文件列出冲突块编号，按「文件 + 块号」写回。若将来要做，
  记在这里而不是 §3（它不需要 URI 体系）。

### 4.6 运行时插件 / 市场 / 自定义工具

- 上游：`extensibility/plugins/{manager.ts 1336, loader.ts 497, installer.ts 169, parser.ts 107}`
  （核心是 `bun install` + `node_modules` + 运行时 `import()`）、
  `extensibility/plugins/marketplace/{manager.ts 1175, fetcher.ts 323, registry.ts 200, cache.ts 136}`
  （核心是 git 克隆 catalog + 缓存）、`extensibility/custom-tools/{loader.ts 313, types.ts 297}`
  （运行时加载 TS 模块，注入 `exec` / `ui` / `logger` / schema 库）；
  扩展本体是运行时 TS 工厂（`extensibility/extensions/{types.ts 2225, runner.ts 2254, wrapper.ts 535}`）。
- 判「不做」：prux 是**编译期静态注册**（`linkme` 分布式切片 + `Extension` trait），
  没有运行时模块加载器，也不打算有。沿用 kiss / jcode 两档的同一结论。
  可借鉴的**非功能**细节：omp 的 `disabledExtensions` 用了统一 id 命名空间
  （`extension-module:` / `context-file:` / `skill:`），值得 prux 在做 §3.2 时参考，
  以便细粒度关闭导入项。

### 4.7 memory 多后端（hindsight / mnemopi / sharpshooter）与 `learn`

- 上游：`docs/memory.md` 给出五选一 `memory.backend`：`off`（默认）/ `local` / `hindsight` /
  `mnemopi` / `sharpshooter`。`local` 是两阶段管线（`memories/index.ts` 1562、
  `memories/storage.ts` 598），写 `MEMORY.md` / `memory_summary.md` / `skills/`；
  `hindsight` 是远程服务（默认 `http://localhost:8888`）；`mnemopi` 是本地 SQLite + 本地
  embedding server（`mnemopi/state.ts` 1262、`backend.ts` 747）；`sharpshooter` 是三份决策文件。
- 判「不做」：prux 的 `memory` 扩展是 BM25 **词法**召回（`src/extensions/memory/recall.rs` 开头
  明确写着「prux 没有 embedding 栈」，沿用 `migration-jcode-v0.94.0.md` §4.15 的结论）。
  远程服务与本地 embedding 都是**新增重依赖**，不做。
- `learn`（`src/tools/learn.ts` 180）依赖 memory 后端与 `autolearn.enabled`；
  prux 侧「沉淀技能」由 §3.6 的 `manage_skill` 覆盖，**不另起扩展**。

### 4.8 DAP 调试器（`debug` 工具）

- 上游：`src/tools/debug.ts`（986）+ `src/dap/`（3973：`session.ts` 1876、`client.ts` 1060）；
  文档 `docs/tools/debug.md`；对外 28 个 DAP op；需外部 adapter
  （`src/dap/defaults.json`：`gdb -i dap` / `lldb-dap` / `codelldb` / `debugpy`，stdio 或 socket）。
- 判「不做」的**理由不是「做不到」而是「不该先做」**：扩展理论上能 spawn adapter 并说 DAP，
  但它是 4000 行量级的子系统、平台相关、且要配套的 UI（栈帧 / 变量 / 断点面板）。
  与 §3.15 的 LSP 相比，DAP 的日常收益低得多，而成本同级。放在 §3.15 之后作为「若有需求再说」。

### 4.9 桌面控制（`computer`）

- 上游：工具壳 `src/tools/computer.ts`（337）+ `computer/worker.ts`（1887）+ `supervisor.ts`（685），
  原生层 `crates/pi-natives/src/desktop/` **56803 行**（macOS AX/CGEvent、Linux X11/Wayland、Win32 三套）；
  而且它**只是 eval prelude，不是 AgentTool**（`computer.enabled` 默认 false）。
- 判「不做」：平台原生层是巨量代码，且绑在 eval 运行时上。prux 已有的 `browser`（CDP）
  覆盖了「驱动浏览器」这一半；桌面这一半不做。

### 4.10 `security_scan` 与 `ida`

- `security_scan`：`src/tools/security-scan.ts`（289）+ `src/security/`（4887），
  绑 omp 的 Codex Security 云控制面与 ChatGPT OAuth（含 `security://` store）。**不做**（需服务端）。
- `ida`：`src/tools/ida.ts`（248）+ `src/ida/`（2077）+ `worker.py`，需**商业 IDA Pro** 与 idalib。
  **不做**（依赖商业软件、受众极窄）。

### 4.11 内联 shell 与 coreutils（`pi-shell` + `pi-builtins`）

- 上游：`pi-shell`（42280 行：常驻 brush shell + 执行/取消/超时/进程树清理/输出最小化）、
  `pi-builtins`（112177 行：brush POSIX builtin 分叉 + 71 个 utility builtin，含 ls / sed / sort /
  xargs / jq / grep / fd / diff / ps / top / kill），走 `Host` trait 零 fork/exec。
- 判「不做」：prux 的 `bash` 是**外部 shell**（`src/core/tools/bash.rs` 直接 spawn），
  语义与内联解释器不同，替换等于重做 shell。体量与耦合（brush `Host` trait、uucore）都不合适。

### 4.12 `pi-vcs` / `pi-walker` / `pi-ast` / `pi-diff` / `pi-iso` 作为依赖整体引入

- 上游规模：`pi-vcs` 13872（gix 0.85 + jj-lib 0.44）、`pi-walker` 7933（依赖 `pi-vfs`）、
  `pi-vfs` 6466、`pi-ast` 6145、`pi-iso` 4845（CoW 克隆 + tree diff）、`pi-diff` 1019（**零依赖**）。
- 判「不做（整体引入）」：
  - 这些 crate **未发布到 crates.io**（workspace `version.workspace`），只能 vendoring / path 依赖；
    且 oh-my-pi 的 `[patch.crates-io]` 分叉（brush / napi / tree-sitter-go）只有留在同一 workspace 才生效；
  - `pi-walker` + `pi-vfs` 解决的是「prux 的 grep/find 要 shell out 到 rg/fd」这件事——
    这是**真实缺口**（`src/core/tools/grep.rs:100`、`find.rs:78`），但它是**内置工具的实现替换**，
    属 core 而不是扩展。若要做，建议**另开一份迁移记录**（不是本档）；
  - `pi-diff` 是本档里唯一「零依赖、可直接抄」的一个（1019 行、纯函数、jsdiff v9 语义）。
    但 prux 已用 `similar` 出 diff（`src/utils/edit_diff.rs`），换掉它的收益只有「输出逐字节对齐 omp」，
    没有用户可见价值——**不做**；
  - `pi-vcs`（gix + jj）会让 `Cargo.lock` 显著膨胀，而 prux 现在靠外部 `git` 就够；
  - `pi-iso` 是文件系统隔离层（APFS/btrfs/ZFS/reflink/overlayfs/ProjFS），
    prux 的子代理隔离用 git worktree 即可（`subagent` 扩展已有 worktree 隔离）。

### 4.13 输入补全（`pi-predict`）与语音（`pi-voice`）

- `pi-predict`（8284 行）：输入框的「下一词」补全（ngram 为主，SmolLM2-135M 可选，macOS 拼写）。
  prux 的 `Suggestions` hook 只覆盖 `@` 候选（`suggestions(query, line_prefix)`），
  **没有行内 ghost text 的扩展接口**，而且补全属于 TUI 输入层。**不做**。
- `pi-voice`（3708 行）：麦克风采集 / 播放 + Opus / WebRTC 实时通话。prux 无音频基建，**不做**。
- 相关的 `tts` 工具（`src/tools/tts.ts` 233 + `src/tts/` 2434）：需要语音 provider 协议与播放链路，
  prux 的 provider 层没有 speech API。**不做**（若将来要做，属「新 provider 协议」，不是扩展）。

### 4.14 用户可配置的 shell hooks

- 上游：`docs/hooks.md` + `extensibility/hooks/{types.ts 601, loader.ts 243, runner.ts 440, tool-wrapper.ts 150}`：
  用户在配置里写 shell 命令，挂在生命周期点（含 `user_bash` / `user_python` 覆盖结果）。
- 判「不做」：属 **core 的扩展机制**（与 prux 的 Rust hook 面是同一个位置），
  沿用 `migration-jcode-v0.94.0.md` §4.2 的结论。若认为 prux 的 hook 面缺项，
  应当**给 `ExtensionHook` 补变体**（§6.2 列了几条候选），而不是新起一个扩展。

### 4.15 provider 级能力（角色路由的 fallback chains / 凭据轮换 / path-scoped models / 60+ provider）

- 上游：四档路由（`retry.fallbackChains` 的 429 回退、每 provider 多把 key 的
  session-affinity 轮换、`enabledModels` / `disabledProviders` 的 `path:` 前缀作用域、
  自定义 provider 的十几种 `api`）、九个模型角色（`default` / `smol` / `slow` / `plan` /
  `commit` / `vision` / `task` / `advisor` / `tiny`）/ 60+ provider / 约 1000 模型。
- 判「不做（作为扩展）」：这些都在 **provider 请求路径**里，而 prux 的
  `ProviderStreamEvent` 只能**只读**观察（不能改载荷、不能换 provider、不能重放），
  也没有多凭据概念。「多 provider 适配」属 provider 层的新适配（catalog 数据 + 协议实现），
  不是扩展；「角色路由」prux 已有 `virtual-models` + `jet` 的近似物（强模型规划、便宜模型实现），
  omp 的 `prewalk` 亦同此意（§4.16）。

### 4.16 其余零碎（逐条一句话）

| 项 | 上游 | 为什么不新起扩展 |
|---|---|---|
| `todo` 工具 | `src/tools/todo.ts` 770 + `src/session/todo-tracker.ts` | prux 的 `tasks` + `plan-mode` 已覆盖「会话待办 + 阶段 + 提醒注入」；omp 多出的「阶段」与「从 Markdown 导入」可作为 `tasks` 的**小切片**而不是新扩展 |
| `prewalk` | `docs/prewalk.md` | prux 的 `jet` 就是「强模型规划 → 便宜模型实现」，同一条功能；不重复 |
| `/tan`（旁支任务后台跑完整 agent） | `builtin-lifecycle.ts:453` | 属 `subagent` 的辅助项（后台子代理），已由 `subagent` 的 `schedule` / 后台运行覆盖 |
| `/pause`（冻结全部 agent） | `builtin-control.ts:77` | 需要一个全局「暂停」闸门，属 core 的运行控制；扩展只有 `AbortRun` |
| `/force`（强制下一轮用某工具） | `builtin-control.ts:9` | 需要 core 的 tool_choice 支持；`filter_tools` 能做的近似（只暴露一个工具）语义不同、易误伤 |
| `/guided-goal`（访谈后设目标） | `builtin-modes.ts:404` | 属 `goal` 扩展的子命令，不新起扩展 |
| `ci-green`（CI 绿灯为止的打包命令） | `extensibility/custom-commands/bundled/ci-green` | 是「提示词 + 循环驱动」的组合；prux 侧由 §3.12 的 `loop` 或 `prompt-templates` 覆盖，不单独立项 |
| `/git`（暂存 + 提交编排 UI） | `builtin-session.ts:580`、`src/commit/`（agentic 拆分原子提交） | 与 prux 的 `/giff`（只读审阅视图）**方向不同但位置相同**；要做应扩成 `/giff` 的可写档，而不是并列一个新视图。属大改动，暂不做 |
| `/hub`、`/activity` | `builtin-session.ts:591`、`src/activity/` | prux `subagent` 已有 FleetView / dock 名册 |
| `/dump`、`/anon`、`/trace` | `builtin-collaboration.ts:231-243` | prux 已有 `/export` / `/share` / `bug`（诊断 zip） |
| `/compress`（把文本改成密集提示词语域） | `src/compress/` | 一次性 agent 管线，收益窄；不做 |
| `/ratchet`（为某个 LLM 流程造 eval 并爬山优化） | `builtin-modes.ts:735`、`src/ratchet/` 1293 | 依赖 eval kernel（§3.14 都不一定做）+ 评分/冻结集机制；受众是提示词工程，不做 |
| `/ssh`（SSH 主机管理 + `ssh://` 路径） | `builtin-lifecycle.ts:137` | 依赖 VFS 与 `ssh://` scheme（§4.5） |
| `secrets`（`secrets.yml` 的密钥混淆占位符） | `docs/secrets.md` | 属 core 的配置/环境机制，不做扩展 |
| shell completions 自动生成 | `README.md`「Shell completions」 | 属 CLI（prux 用 clap，配 `clap_complete` 即可），不是扩展 |
| `wait`（阻塞等下一个后台结果 / 对端消息 / steer 中断） | `src/tools/wait.ts` 190 + `src/async/job-manager.ts`、`src/irc/` | prux 有 `register_background_cancel` 与子代理回执，但**没有 async job manager / 对端（IRC）概念**；单做一个 `wait` 工具会变成一个永远等不到东西的接口。属基建缺失，不做 |
| `context_notes` / `new_context`（实验性上下文压缩辅助） | `src/tools/context-notes.ts`、`new-context.ts` | 绑 core 的压缩管线与 `new_context` 会话原语，属机制级改动 |
| `import`（从别的 agent 导入会话） | 各 `discovery/*.ts` 的会话读取分支 | 与 §3.2 同源；会话格式互转风险高、收益存疑，只做配置不做会话 |

---

## 五、落地后的收尾检查（**通用清单**）

每落地一项后逐条过：

1. **声明位置**：新扩展 = `src/extensions.rs` 加 `pub mod x` + 模块内 `#[linkme::distributed_slice]`
   工厂静态 + 在 `PRIORITY_*` 常量区加一条（**不要**改 `main.rs` 的注册函数）；
2. **默认态**：新扩展一律 `default_enabled() == false`，需 `/extension` 面板或
   `enabledExtensions` 显式开启；不要抢已有行为，也不要在注册时就发 UI 请求；
3. **依赖方向**：扩展只依赖 `core`（trait 面）与 `utils`；反向依赖（core 引扩展）= 违规；
4. **锁序**：注册表 → 扩展 state；配置缓存「RwLock → 缓存 Mutex」；
   回调里不跨 `push` / `retain` 持锁；**回调体内不再取任何锁**；
5. **注释**：`struct` / `enum` / 字段 / 变体 / `const` / `static` / `type` / 全部 `fn` 都要中文 `///`；
   搬代码 = 搬注释，搬完回头确认原处没有孤儿注释；
6. **enum ↔ 字符串**：用 `strum`（`EnumString` / `IntoStaticStr` / `EnumIter`），不手写转换；
7. **联网测试**：网络请求用 `test_support` 的 `run_with_timeout` 包住；
   测试隔离只用线程本地 override（`AgentDirGuard::temp()` / `HomeGuard` / `pin_test_agent_dir`），
   禁用 `set_var("PRUX_AGENT_DIR")`；
8. **文档**：用户可见能力写进 `assets/docs/extensions.md`（新扩展的表格行 + 需要时单开一节），
   **不要**在 `assets/docs/**` 里写移植叙事、上游文件名、或 `src/` 之类 AI 读不到的路径；
9. **changelog**：在 `assets/changelog/v.<Cargo.toml version>.md` 追加一行条目；
10. **格式化与静态检查**：`cargo fmt` → `cargo check` → `cargo clippy` → `cargo test`
    （每次提交前都跑一遍；纯格式化与逻辑改动分开提交）；
11. **提交**：中文、按 `[+]` / `[-]` / `[*]` 逐行前缀、显式路径 `git add <file>`、
    不 `git add -A`、行间不留空行。

---

## 六、复核记录与待确认项

### 6.1 已核对的事实与易错点

| 事实 | 依据 |
|---|---|
| prux 的 `grep` / `find` **不是**内置实现，而是 shell out 到外部 `rg` / `fd` | `src/core/tools/grep.rs:100`（`tools_manager::rg_path()`）、`find.rs:78`（`fd_path()`）、`src/core/tools_manager.rs` |
| prux **没有**任何 tree-sitter / LSP / DAP / Python 依赖 | `Cargo.toml`（已通读）；`src/` 内无相关引用 |
| prux 的 `codemode` 每次调用**新建** QuickJS 运行时，跨调用不保留状态 | `src/extensions/codemode/sandbox.rs:231`（`AsyncRuntime::new()` 在 `run_inner` 内） |
| prux 的 `memory` 是 BM25 **词法**召回，无 embedding | `src/extensions/memory/recall.rs:1-5` 注释 + `BM25_K1` 常量 |
| prux **能**渲染内联图片（kitty / sixel / iTerm） | `src/modes/interactive/render/image.rs`（`ratatui_image`，`ProtocolType`）、`utils/terminal_image.rs` |
| prux 的 provider 层**已有**图像生成实现（OpenAI 兼容） | `src/core/provider/images.rs`（`generate_images`、`modalities`、`data:` URL 解析）；`Extension::image_apis()` / `classifier_apis()`（`src/core/extensions.rs:457`/`465`） |
| prux 的 `commands()` 由框架**每次查询都重新调用**，可以按工程动态返回 | `src/extensions/plan_mode.rs:844-849` 的「残留轮询面」注释 |
| prux 的 TUI 每帧处理**一个**扩展 UI 请求 | `src/modes/interactive.rs:339`、`maybe_handle_extension_ui`（`:1080`）、`take_pending_ui` |
| prux 的 `before_tool_call` 是同步的，只能 block + reason | `src/core/extensions.rs:841`、`:862`（`before_tool_call_ext`） |
| prux 的 `ProviderStreamEvent` 只能缓冲观察（归一化之前、返回 `()`） | `src/core/extensions/hooks.rs`（该变体注释）、`src/core/extensions.rs:667`（`fn provider_stream_event(&self, _event: &Value) {}`） |
| prux 已有 `.agents/skills` 与 `~/.agents/skills` 发现 | `src/core/skills.rs:534-546` 的加载顺序注释 |
| prux 的上下文文件候选名**只有 5 个** | `src/core/context.rs:10`（`CANDIDATES: [&str; 5]`） |
| prux 已有 `--exclude-tools` / `--no-tools` / `--no-builtin-tools` | `src/cli/args.rs:114-126` |
| oh-my-pi 的工具注册表共 30 个内置工具 | `packages/coding-agent/src/tools/builtin-names.ts` |
| `ast_grep` / `generate_image` / `manage_skill` / `tts` 等在 omp 侧**默认关闭** | `docs/tools/ast-grep.md`（`astGrep.enabled = false`）、`generate_image.md`（`generate_image.enabled=true` 默认 false）、`manage_skill.md`（`autolearn.enabled = true` 默认 false） |
| `ast-grep-core` 在 crates.io（0.51.0，MIT）；`calamine`（0.37.0）、`docx-rs`（0.4.22）存在 | 本轮 crates.io 检索 |
| omp 的 `computer` 不是 AgentTool，而是 eval prelude | README §21 与 `src/tools/computer.ts` 的头注释 |
| omp 的 `browser` 同样是 eval prelude（`createBrowserPrelude`）；`generate_image` / `tts` 是 SDK 注入的 `CustomTool`，三者都**不在** `BUILTIN_TOOL_NAMES` 里 | `tools/browser.ts:251`、`tools/image-gen.ts:236`、`tools/tts.ts:176`、`tools/builtin-names.ts` |

**易错点**（提醒后来人别踩）：

- **omp 的 `rewind` 与 prux 的 `rewind` 不是同一个东西**。omp 的 `rewind(report)` 是
  「折叠探索过程、只留一份报告」（`src/tools/checkpoint.ts`）；prux 的 `/rewind` 是
  「撤回最后一次用户输入」（`src/extensions/rewind.rs`）。**不要**把 checkpoint/rewind 当成
  「prux 已有」而漏掉，也不要直接复用 prux 的 rewind 实现。
- **omp 的 `browser` 也不是 prux 的 `browser`**：omp 的在 eval prelude 里（`tab.run(...)`），
  prux 的是一个 16 动作的 `browser` 工具。判「已覆盖」时要按 prux 的形态算。
- **`glob` 与 prux 的 `find` 冗余**（prux `find` 就是 fd 式 glob 查找），不要为它新起扩展。
- **`pi-diff` 零依赖、可以直接抄**——但「能抄」不等于「该抄」（见 §4.12）。

### 6.2 落地前需要确认的决策点

1. **`ast_grep` 的语法集**：内置哪些？是否接受「冷门语言报错」而不是按需下载 wasm？
   （本档建议：只内置 ~20 种主流语言，其余明确报错；避免联网下载编译期资源。）
2. **`agent-import` 的信任与开关**：读取用户项目里的 `.cursor` / `.github` / `.vscode` 是否
   一律要求项目信任？逐来源 opt-in 的键名与默认值？（本档建议：要求信任 + 默认全关。）
3. **`agent-import` 的物化策略**：是「每次启动把外部格式同步成 SKILL.md / prompt 模板」（幂等、会写盘），
   还是「只在内存里并入本次请求的上下文」（不写盘、但技能面板看不到）？
   （本档倾向：**前者用于 skills / prompts，后者用于 rules / context**。）
4. **`ask` 的可行性验证**：工具执行期间伪造一次 Select 回传，确认能 await 到答案（§3.5）。
   若不通，本项降级为「模型只能靠 `request_ui` 单向通知」。
5. **`ultrathink` 是否值得为它动 core**：加一个「单轮 thinking 档」的扩展动作（§3.11）。
6. **`cleanse` 的 checker 来源**：自动发现 vs 用户显式给命令。（本档建议先做后者。）
7. **`advisor` 的注入策略**：只注入 `blocker` 档（省 cache），还是三档都注入（更吵但更主动）？
   需要一个 `nit` / `concern` / `blocker` 的枚举（用 `strum` 做字符串互转）。
8. **是否给 `ExtensionHook` 补变体**：本档在核对中发现 omp 有几个 prux 缺的 hook 点，
   如果将来有功能需要它们，应当**补 hook 而不是绕路**：
   `before_provider_request`（改载荷）、`before_subagent_spawn`（子代理模型路由 / 阻断）、
   `assistant_message`（改写落库文本）、`session_before_{switch,branch,tree}`、
   `user_bash` / `user_python`（覆盖工具结果）。**本档不要求现在就补**，仅记录为
   「若某项功能卡在缺失的 hook 上，先来这张表查」。
