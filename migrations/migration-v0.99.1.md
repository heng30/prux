# Prux 迁移指南：pi 0.87.1 → 0.99.1

> 本文档仅列出当前 prux 项目**已实现的功能**在 pi 0.87.1→0.99.1 期间的相关变更，
> 以及该区间内 pi **新增的功能**（作为候选移植项）。
>
> 排除规则：prux 没有、且不是 0.87.1→0.99.1 新增的功能，不列出。已刻意省略：
> `packages/{client,protocol,server,telemetry}`（0.99.x 无任何条目）；
> `packages/durable`（0.99.0 整包重写，prux 无 durable 架构，结构性不适用）；
> 纯 JS/Node 运行时改动（TypeScript 7 构建切换、`tsx` 移除）；
> `--mode` CLI、RPC `prompt/steer/follow_up` per-input disposition、`RpcClient` 修复、
> Kitty 图形协议等 prux 架构/平台上不存在的能力
>（macOS Finder 粘贴原先也在此列，第二十轮已落地，见 25.1）；
> llama.cpp provider 相关（autoload presets、llama-cpp-classify）——prux 未实现 llama.cpp；
> Mistral provider 相关修复（GLM 空 delta、reasoning thinking level）——prux 排除 Mistral。
>
> **来源**：`pi/packages/{coding-agent,ai,tui,agent,mcp,codemode}/CHANGELOG.md` 的
> `[0.99.0]`、`[0.99.1]` 段落。注意：pi 从 0.87.1 直接跳版到 0.99.0（无 0.88–0.98 版本），
> 因此全部变更集中在两个 release 段。
>
> **prux 基线**：0.87.1（见 `migrations/migration-v0.87.1.md`）；现状以本仓
> `assets/models/`、`src/core/`、`src/extensions/` 为准。
>
> **状态标记**：✅ 已确认现状/已满足或已实现（无需改动）　⚠️ 需对照 pi 源码逐项核对
> 🔧 应修/应移植　❌ 结构性不适用（仅备注）　**✔️ 已落地**（第二轮已改代码，见第六节）
> 🚫 **不需要同步**（pi 侧移除/重写项，prux 无对应结构或能力，本区间不做）
> ❗**第十九轮复审新发现**（原判「刻意不做/结构性不适用」被推翻，实为待移植/待验证，见第二十四节）
> 🆕 **第三～十二轮已落地**（见第八～十七节；含与 pi 的偏差说明）　📖 **第十三轮为文档轮**（第十八节：2.1 codemode 的实现形式分析，未改代码）
> 🆕 **第十四轮已落地**（第十九节：2.1 codemode + 2.3 余项的 `prepareLoadout()`）
> 🆕 **第十五轮已落地**（第二十节：2.1 余项全清 —— bash 1 MiB 结构化输出、`models.*`、
> `constrainedSampling: grammar`、TUI 嵌套调用渲染；2.2 的 MCP exposure 配置面）
> 🆕 **第十六轮已落地**（第二十一节：远程目录 `types=chat,image,classifier`、内置 dark/light 换
> pi 0.99 的 OKHSL 调色板（含旧键别名）、未知 `type` 条目整条丢弃；顺带修 4 处陈旧断言与 `↳` 占位符）
> 🆕 **第十七轮已落地**（第二十二节：第三次复审 CHANGELOG 找到的 4 项非刻意缺口——
> codemode 模型调用视图渲染、跨 provider 模型集合 API、项目级扩展启停、Markdown 解析结果复用）
> 🆕 **第二十轮已落地**（第二十五节：第十九轮那两行待办——macOS Finder 文件粘贴、
> 扩展注册 MCP 服务器；以及 2.5 的两项取舍——索引兜底档、扩展可读主题色/外观。
> 实时明暗切换试做后按用户指示移除）
>
> 本文档首轮为**纯文档对照审计**；第二轮（第六节）完成第一节 P0–P3 全部代码落地；
> 第三轮（第八节）按指定实现 2.4 / 2.5 / 2.7 / 2.11 / 2.12 / 2.13，并记录与 pi 的偏差；
> 第四轮（第九节）实现 2.6 Sign in with ChatGPT，并按用户拍板**删除** openai-codex；
> 第五轮（第十节）落地一.12 的两项遗留：摘要 usage 进会话投影、`/session` 按模型分列成本。
> 第六轮（第十一节）把缓存保温用量（`UsageRecord`）计入 `/session` 合计与分列。
> 第七轮（第十二节）对齐 MCP 调用渲染与 OAuth 跨进程刷新串行化。
> 第八轮（第十三节）实现未实现项的共同前置（目录 `type` 感知）与 2.3 的扩展工具编排子集
> （`exposure` / `outputSchema`+`structuredContent` / `ctx.execute_tool`+`nestedCalls`）；
> 第九轮（第十四节）实现 2.10 图片生成（provider 层一次性 `generate_images`，
> 对齐 pi `Models.generateImages()`；不含落盘/展示与模型可见工具）。
> 第十轮（第十五节）实现 2.9 分类器（provider 层一次性 `classify`，
> 对齐 pi `Models.classify()`；只做 `typesafe-system-one`）；
> 第十一轮（第十六节）实现 2.8 虚拟模型宿主侧全链路；
> 第十二轮（第十七节）把 pi 的示例插件 `jev-router.ts` 移植为内置扩展 `jet`
> （`/jet set|router|reset`，无默认值，配置有效才注册 `jet/auto`）；
> 第十三轮（第十八节）确定 2.1 codemode 的实现形式——**内置扩展 `codemode` 注册单个
> `exposure = model-only` 的 `codemode` 工具**（扩展名与工具名 1:1），而非 `core/tools` 内置工具；
> 第十四轮（第十九节）按该形态落地 codemode，并一并补上它的硬前置
> `Extension::prepare_loadout()`（2.3 余项之一）；第十五轮（第二十节）清掉 2.1 全部余项；
> 第十六轮（第二十一节）对照 CHANGELOG 复审出三处缺口；第十七轮（第二十二节）再复审出
> 四处非刻意缺口（codemode 模型调用视图、跨 provider 模型集合、项目级扩展启停、Markdown 解析复用）。
> 第十八轮（第二十三节）收尾复审出的剩余缺口：新增 `typesafe` provider、扩展可注册
> image / classifier 模型与协议实现、按 pi 换成数组式模型目录（`models.all.json`）；
> `/proxy` 路由虚拟模型按用户指示不做。
> 第十九轮（第二十四节）为**文档轮**：逐条复核「刻意不做 / 结构性不适用」的判定，
> 订正 7 处过期或写错的理由，并把 2 项真缺口（macOS Finder 文件粘贴、
> `pi.registerMcpServer()`）与 1 项待验证项（#9786 X11 剪贴板）从「不适用」中移出。
> 第二十轮（第二十五节）清掉那两行待办（剪贴板文件路径 + 粘贴优先级、扩展注册 MCP 服务器），
> 并按用户指定落地 B 组偏差（codemode `store()` 按分支、索引兜底档、扩展主题视图）；
> 「系统主题实时明暗切换」试做后按用户指示**不做并移除代码**（8.6 取舍 2 已更新理由）。
> 未实现项之间的依赖关系与建议顺序见 **2.15**。

---

## 一、Bug 修复（prux 已有功能，升级时应评估移植）

### 1.1 ✔️ 默认模型指向已被上游移除的 Kimi K2.6 → 改指 Kimi K3

pi 0.99.0 修复了 Fireworks / OpenCode Go / Together 三个 provider 的默认模型，
它们指向已被移除的 Kimi K2.6（#28fd59086 / e1787702d）。

prux 现状（`src/core/model_resolver.rs::default_model_id`）**曾全部指向旧模型**：

| provider | prux 原默认 | pi 0.99.1 默认 |
|---|---|---|
| fireworks | `accounts/fireworks/models/kimi-k2p6` | `accounts/fireworks/models/kimi-k3` |
| opencode-go | `kimi-k2.6` | `kimi-k3` |
| together | `moonshotai/Kimi-K2.6` | `moonshotai/Kimi-K3` |

同时 prux 内嵌目录里 `models.opencode-go.json`（`kimi-k2.6`）、`models.together.json`
（`Kimi-K2.6`）、`models.fireworks.json`（`kimi-k2p6`）等条目上游已移除，
需随目录同步（见第四节）一并清理。其余 provider（moonshotai / opencode / openrouter /
huggingface）pi 仍默认 kimi-k2.6，prux 不需改。

**实际结论**：目录三者已在 3cff3ca（同步模型文件）里清理完毕（fireworks/opencode-go
已有 `kimi-k3`，together 只有 `moonshotai/Kimi-K3`）→ 本节待改的只剩默认模型表。
已改 `default_model_id` 的三个 provider，并在 `default_model_follows_pi_table` 补断言。

### 1.2 ✔️ 默认 OpenAI Codex 模型改为 GPT-6.1 Sol（0.99.1）

- pi 0.99.1 新增 `gpt-6.1-sol` 到 OpenAI / Azure OpenAI Responses / OpenAI Codex，
  并将 **openai-codex 默认模型** 从 `gpt-5.5` 改为 `gpt-6.1-sol`。
- prux 现状：`default_model_id("openai-codex") == "gpt-5.5"`（有回归测试断言），
  `assets/models/models.openai-codex.json` 无 gpt-6.1-sol。
- 同时 pi 将 OpenAI Codex provider **改名为 "OpenAI Codex (legacy)"**
  （Sign in with ChatGPT 取代之）；prux 第四轮已删除 `openai-codex`（见三.3/第九节），
  ChatGPT 订阅登录改挂在 `openai` provider 上。

**实际结论**：目录已含 `gpt-6.1-sol`（openai 与 openai-codex）；已改默认模型并补断言。
provider 改名归三.3（本轮未做，属体验/文案层，不与 pi 运行时行为矛盾）。

### 1.3 ✔️ OpenAI Responses 流缺少 `output_index` 时静默混排工具调用 → 改为报错

pi #9974（1b2aa0ca0）：省略 `output_index` 的服务器（如 llama.cpp）会让未完成的
工具调用被当成可运行的，导致命令混跑；现在此类流以错误终止。

prux 现状（`src/core/provider/responses.rs:572`）：
`ev.get("output_index").and_then(as_u64).unwrap_or(0)` —— **与修复前的 pi 行为一致**，
缺失时静默当作 0。应对照 pi 修复改为报错终止。

**实际结论与实现偏差**：pi 的修复是「流末检查未定稿工具调用」（`partialJson !== undefined`），
但 prux 的 slot 表按 `output_index` 索引且**后者覆盖前者**，混排后旧块直接丢失，
单靠流末检查拦不住。因此改为两条拦截：
1. `response.output_item.added` 命中未定稿的同名索引 → 立即报错
   （`reused output_index N`：不合规服务器在首个冲突处就报错）；
2. 流结束时仍有未定稿工具调用 slot（没收过 `output_item.done`）→ 报错
   （`unfinished tool call: <name> (<id>)`）。

### 1.4 ✔️ OpenCode Zen/Go `qwen3.8-flash` thinking 空签名 → 后续轮次重放为纯文本

pi #10047（c1449660c）：端点返回空 thinking signature，导致 thinking 块在后续轮次
被当作纯文本重放。prux 有 `opencode`（= OpenCode Zen）与 `opencode-go` provider，
`models.opencode.json` 含 `qwen3.8-flash` → **同一缺陷面存在**，需对照 pi 的
空签名跳过逻辑移植。

**实际结论：无需改代码**。pi 的修复就是给目录条目加 `compat.allowEmptySignature`
（`generate-models.ts:1230`），而 prux 目录已带该字段，且 `anthropic.rs` 的
`compat_allow_empty_signature()` 在保存重放时已按它保留空签名（pi #9323/#9676 轮实现）。
本次补回归测试 `qwen38_flash_empty_signature_replayed` 锁住该行为。

### 1.5 ✔️ `@`/路径自动补全在 `(`、`[`、`{`、`<`、反引号等包装符后失效

pi tui 0.99.0：`(~/Dev<Tab>` 这类输入不再补全。
prux 现状（`src/modes/interactive/handlers/suggest.rs:729`）：
`is_autocomplete_separator = 空白 || CJK 标点` —— **不含 ASCII 包装符**，
`(~/Dev` 会把整段当一个词，行为与修复前的 pi 相同。应移植 pi 的分隔符集合。

**实现**：按 pi d5629e204 的「剥包装符」而非「把包装符当分隔符」语义移植：
`PATH_WRAPPERS`（`(` `[` `{` `<` 反引号）→ `strip_leading_wrappers`（token 内已含配对
闭括号时保留，避免误伤 `app/[slug]/pa`）+ `is_at_token_start`（`(@src` 视为 token 起始，
邮箱 `user@example.com` 仍不触发）；`last_word` 的起始字节索引同步指向包装符之后。

### 1.6 ✔️ Vercel AI Gateway 1 小时 Anthropic cache write 按 5 分钟价计费

pi #9210：网关在流式 delta 中上报 1h cache write，被按 5min 价计。
prux `src/core/provider/usage.rs` 已有 `cache_write_1h` 通用计费结构，
但 `completions.rs`（OpenAI 兼容路径，Vercel 网关走此路）解析时
`cache_write_1h: None` → 若网关上报 1h 写入，prux 同样低估成本。
需核对 pi 在 OpenAI 兼容流里解析 1h cache write 的位置并移植。

**实际结论：本节诊断路径有误，但缺陷真实存在**。pi 的修复在
`anthropic-messages.ts`（Vercel 网关的 Anthropic 模型走 anthropic-messages），
不在 OpenAI 兼容路径：prux `anthropic.rs::handle_anthropic_message_start` 已读
`cache_creation.ephemeral_1h_input_tokens`，但 **message_delta 不读**，
而网关只在 delta 里带 TTL 拆分 → 同缺陷。已在 `handle_anthropic_message_delta`
补读 `cache_creation.ephemeral_1h_input_tokens`（delta 里已有 `cache_creation_input_tokens`
→ `cache_write` 的解析）并加回归测试。

### 1.7 ✔️ OpenAI Fast mode（`service_tier: "fast"`）按标准价计费

pi #10034：GPT-6 系列响应 `service_tier: "fast"` 时应按 fast 价计。
prux `usage.rs` 不读 `service_tier`。prux openai 目录已含 gpt-6-astra/luna/sol，
若这些模型在 prux 下触发 fast tier，同样低估成本。需对照 pi 的 fast 价目来源移植
（注意 prux 的 `extensions/proxy/openai.rs` 会强制 `service_tier: "auto"`，
代理路径不受影响，直连路径需修）。

**实现**：新增 `usage::service_tier_cost_multiplier` / `apply_service_tier_cost`
（对齐 pi：flex 0.5；priority/fast 为 gpt-5.5 时 2.5 否则 2.0；其余 1.0），
在 `responses.rs` 的 `handle_terminal_response` 读取响应体的 `service_tier`，
由 `build_responses_result` 在 `compute_cost` 之后缩放（openai-codex 走同一路径）。

### 1.8 ✔️ 浏览器 OAuth：授权错误时无限等待；Anthropic 回调端口被占用无兜底

pi ai 0.99.0：Anthropic / OpenAI Codex 浏览器登录在 provider 以 authorization
error 重定向后不再无限等待，改为带错误描述失败；Anthropic 回调端口被占用时
回退为粘贴 redirect URL。

prux 现状：`src/core/oauth.rs` 有共享 `CallbackServer`（与 pi 0.99.0 统一回调
服务器方向一致）；`oauth/anthropic.rs` 端口占用时仅报错提示
（"Close the process holding the port..."），**无粘贴 URL 兜底**；
回调是否处理 `error` 查询参数需逐 handler 核对。

**实现**：
1. `poll_callback` 命中 `error` 参数时不再「写错误页 + continue」（旧行为＝无限等），
   改为带 `error_description`（缺省回退 `error`）立即返回 `Err`，
   事件循环据此取消登录（`oauth_flow::poll` 的 Err 分支已有）；
2. `LoginKind::AuthorizeCode.server` 改为 `Option<CallbackServer>`：
   anthropic（53692）与 openai-codex（1455）端口被占用时 `server=None`，
   退化为纯粘贴 redirect URL（登录面板本就常驻粘贴提示）；OpenRouter 用临时端口，
   保持必绑。

### 1.9 ✔️ GitHub Copilot Claude Opus 5.5 元数据不全时提供不受支持的 thinking levels

pi：上游模型元数据不完整时现在只提供 low..max。
prux `models.github-copilot.json` 含 `claude-opus-5.5`；thinking level 集合的
推导逻辑需对照 pi 确认是否同样依赖上游元数据、缺失时是否越界。

**实际结论：目录已带 pi 的修正值，无需改代码**。`claude-opus-5.5` 的
`thinkingLevelMap` 已含 `off/minimal: null` + `low..max`，
`supported_thinking_levels()`（缺失键默认支持、xhigh/max 需显式映射）
推出恰好 low..max。补回归测试 `opus_5_5_offers_low_through_max` 同时锁住
anthropic 的 `claude-opus-5-5` / `claude-sonnet-5-5`。

### 1.10 ✔️ 自定义主题忽略 `terminal.trueColor` 等终端能力覆盖，按 256 色渲染

pi #9973：主题渲染需尊重终端能力覆盖。prux 有 `utils/terminal_caps.rs`
（`PRUX_TRUE_COLOR` 覆盖 + COLORTERM 探测）与主题体系
（`modes/interactive/theme.rs`，vars 为 hex 颜色），但未见主题渲染路径
按 `true_color` 降级/选择色彩空间的逻辑 → 大概率存在同一问题，需核对。

**实际结论：缺陷成立且已修**。`theme.rs` 的 `fg()/bg()/color()` 无条件发 24bit，
`TerminalCapabilities.true_color`（含 `PRUX_TRUE_COLOR` 覆盖）此前在渲染侧完全未被使用。
现改为按能力分派：真彩→`38;2/48;2` 与 `Color::Rgb`；否则→`38;5/48;5` 与
`Color::Indexed`，配色近似用 pi `colors.ts::rgbToAnsi256` 的立方体 + 24 级灰阶加权算法
（`rgb_to_ansi256`）。另按 pi 把 `TERM=*-direct` 计入真彩探测。分别是：
`fg_with/bg_with/color_with`（可注入能力，便于单测）。

### 1.11 ✔️ 主题切换后 startup header / 聊天通知残留旧色；流式渲染 CPU 占用

- pi：主题切换后旧色残留（startup header、loaded resources、chat notices）修复；
  流式长会话 CPU 下降（footer 缓存 usage 合计、折叠 bash 结果缓存预览、
  `sanitizeBinaryOutput()` 不再逐字符切分）。
- prux 已有 `/reload` 重载主题（`handlers/commands.rs:261 reload_theme`）与
  footer/bash 渲染，两项均需在 prux 渲染路径上逐条核对复现。

**核对结论（旧色残留）：prux 结构性不存在该缺陷**。主题切换只换 `app.theme`，
所有颜色在渲染时解析：startup header / 聊天通知的 `SysSpan` 存的是主题**键**
（`fg: Option<String>`）而非颜色，颜色由每帧 `theme.resolve_color` 现算；
消息级渲染缓存的失效键已包含 `theme.render_fingerprint()`
（`render/history.rs::ensure_cache_params`），主题变化即整体重建 → 无旧色残留。
补 `derived_render_inputs_survive_theme_change` 锁定行为。

**核对结论（流式 CPU）：采纳两项与 pi 对应的修复**（其余项在 prux 结构上不成立，
见下）：
1. **跨帧派生渲染输入缓存**：`scan_tool_results`（克隆全部工具输出文本）与
   `edit_map_from_results`（克隆全部 diff）此前**每帧**重建，长会话下与
   pi 的 footer 全量扫描同类。现改为按消息集指纹
   （`messages_fingerprint`：消息数/时间戳/角色/块数/…+系统消息）缓存到
   `App::derived_tool_results` / `derived_edit_map`，宽/主题/展开态变化不失效。
2. **`sanitize_output` 快路径**：无 `ESC`、无 tab（绝大多数工具输出）直接整段拷贝，
   不再逐字符跑状态机（对应 pi `sanitizeBinaryOutput` 不再逐字符切分）。

不采纳的理由：prux 与 pi 的渲染模型不同——pi 每帧全量重渲染所有组件（所以需要
footer/bash 结果级缓存），而 prux 已逐消息缓存（`msg_cache` 按 `(timestamp, seq)` 命中），
折叠 bash 预览/消息 markdown 每帧只重渲染流式那一条；footer 的 usage 汇总是纯算术循环
（无 pi 那种 `getEntries()` 全量拷贝）→ 保留现状。

### 1.12 ✔️ 扩展嵌套工具调用（`ctx.executeTool()`）的 usage 未计入会话成本

pi：codemode 脚本经 `ctx.executeTool()` 调用的工具 usage 现计入调用方结果。
prux 扩展 trait 有 `execute_tool` / `execute_tool_async`
（`src/core/extensions.rs:650`），但未见嵌套调用 usage 归集到会话成本的逻辑。
若 prux 扩展（如 tasks/subagent）存在同类嵌套调用，需补归集。

**实际结论：归集链路已存在，缺口只在「统计口径」**。prux 已有
`afterToolCall` → `AfterToolCallOutcome.usage` → `FinalizedToolCall.usage`
→ toolResult 消息 `usage` → 落盘（subagent 的 `reportUsage` 把子代理开销池挂到下一个
工具结果上，与 pi 同形态）。但 footer 统计只累加 `role == "assistant"`，
toolResult 上的嵌套开销被丢弃 → 用户在底栏看到的会话成本漏掉这部分。
现提取共用口径 `footer::scan_usage`（normal / minimal / rich 三处共用）：
除 assistant 外累加带 usage 的 `toolResult` / `compactionSummary` / `branchSummary`，
`latest_cache_hit_rate` 仍只由 assistant 决定。

> 遗留（第五轮已落地，见第十节）：pi 的 `/session` 按模型分列成本（`getUsageCostBreakdown`，
> 含 `Tools/summaries` 桶）已移植；`compactionSummary` / `branchSummary` 的 usage 在 prux
> 落入 `session_manager` 投影时被丢掉（`summary_message` 不带 usage）也已修——投影消息现在带上条目 usage。

### 1.13 ✅ 已确认 prux 无此问题 / 结构性不受影响

- **#10000 首条 assistant 响应前退出丢失新会话**：pi 改为发送首条用户消息时即建
  会话文件；prux `Session::create_with` 在创建时就写 header 并落盘
  （`session_manager.rs` `std::fs::write(&file, header.encode())`），比 pi 修复得更早。
- **#9996 全文件 read 调用被渲染成 `:1`**（模型传 null offset/limit）：prux 的
  `tool_call_header` 只渲染 path 与 limit，从不渲染 offset 起始行号 → 结构性不受影响。
- **#9506 OpenAI 兼容 API 直连 stream()/complete() 丢弃模型级 samplingParams**：
  prux `serialize_request_body`（completions.rs:806）统一合并 `samplingParams`
  （只补未设键），且只有这一条请求路径 → 已对齐。
- **#9797 图片消息带空 text part 被拒**、**Anthropic OAuth 版本号过期**：
  0.87.1 修复，已在 prux 基线内。
- **#9786 X11 剪贴板文本被误判为图片**：prux X11 路径走 arboard（读 RGBA 失败即
  返回 None），与 pi 自研 X11 实现不同；**待验证**（第十九轮复核：此前标 ✅ 无实测依据，
  既非刻意不做也非结构性不适用——prux X11 同样先试 `get_image()`，见
  `utils/clipboard.rs::paste_image_from_clipboard`）。
- **#9944 `/skill` 自动补全过滤为空**：**已有等价实现**（第十九轮复核，订正旧“不适用”结论）：
  prux 有 `/skill:<name>` 命令候选与裸名匹配修复（`handlers/suggest.rs:361-370`，
  回归测试 `skill_commands_rank_by_bare_name`，对应 pi #9120 同型问题）。
- **#8938 Kitty 图片拉伸**：prux TUI 不做终端图片渲染（图片仅作附件占位符）→ 不适用。
- **#10026 光标残留**：**已有等价物**（第十九轮复核，订正旧归组）：该修复与图片渲染无关，
  是「扩展在退出时关闭覆盖层导致 shell 光标未恢复」；prux `teardown_terminal` 无条件
  `terminal.show_cursor()`（`modes/interactive.rs`）→ 结构性不受影响。
- **#9999 Finder 文件粘贴**：❗**待移植**（第十九轮复核，从旧“不支持 macOS 剪贴板 → 不适用”移出）：
  prux 仅在 Linux 走 `wl-paste`，其余平台（含 macOS）走 arboard，且粘贴路径是
  **图片优先**（`handlers/mouse.rs::paste_clipboard`）——与 pi #9999 的成因同形
  （macOS Finder 复制的文件带的图标被当图片插入）。修法需要自建 file URL 读取
  （pi 用 `NativeClipboard.getFilePaths()`），并在 bash 模式下给路径加引号。见第二十四节。
- **#9863/#9982 git 扩展包 peer deps / pinned ref**：prux 扩展为编译期内置
  Rust 扩展，无 npm/git 包加载器 → 不适用。

---

## 二、新增功能（prux 没有，0.87.1→0.99.1 新增，候选移植项）

按移植价值粗排。每项标注 prux 最近似的现有结构。

### 2.1 🆕 Codemode —— **已落地**（第十四轮，见第十九节；形态定案见第十八节）

pi 0.99.0（8562bcf66）：内置 `codemode` 扩展——模型写 JavaScript，在 QuickJS
worker 沙箱里以 `tools.<name>(args)` 并行调用 pi 的工具；由 `defaultTools`
或 `--tools` 启用，`codemode.mode` / `codemode.inlineBudget` 配置；
`models.classify()` 调用的 token/成本计入 codemode 结果与会话成本。
bash/powershell 结构化结果为 codemode 提供至多 1 MiB 输出
（`truncated` + `full_output_path`，保留首尾各 512 KiB）。

prux 现状（**两处前置已就位，此前的“缺 JS 沙箱”判断有误**）：

- **JS 沙箱已有**：`rquickjs` 已是依赖（`Cargo.toml`），且
  `src/extensions/subagent/workflow/{bridge.rs,workflow.rs,glue.js}` 已有完整的
  AsyncRuntime + 宿主桥（`__dispatch`/`__done`/`__settle`/`__progress`/`__budgetSpent`、
  中断上界、确定性前奏、约 30 分钟墙钟兜底）。codemode 是**换一套 JS 侧 glue、
  把桥接目标从 `agent()` 换成 `tools.<name>()`**，不是从零造沙箱。
- **bash 结构化输出大半已具备**：`core/tools/bash.rs` 已返回 `truncated` +
  `full_output_path`（进 `details.truncation`/`details.fullOutputPath`），只差把截断预算
  参数化：现为 50 KiB / 2000 行（`utils/truncate.rs`），pi 的 codemode 路径要
  至多 1 MiB（首尾各 512 KiB）。
- **真正的缺口是工具编排 API**：JS 侧 `tools.<name>()` 的宿主入口就是 2.3 的
  `ctx.execute_tool`（已落地，见第十三节），以及把加载/结果回灌接进沙箱。

建议单独立项评估，但规模应下调（不再是“缺沙箱”的大特性）；可直接复用 workflow 的桥接模式。

**形态结论（第十三轮，见第十八节）**：pi 侧它是**内置扩展**（`builtin:codemode`）
注册的**单个工具** `codemode`（`exposure = model-only`、`defaultActive: false`），扩展名与工具名 1:1，
**不是** `core/tools/` 那批内置工具。prux 已按该形态落地（`src/extensions/codemode.rs`，
`default_enabled() == false` + 一个 `ToolExposure::ModelOnly` 的 `codemode` 工具）。

**落地情况（第十四轮，见第十九节）**：沙箱（rquickjs + JS 侧 glue）、`prepareLoadout()` 等价物、
脚本可调集合拆分、输出截断与 `store()` 持久化都已做；**未做**：`models.*` 全局、
bash 的 1 MiB 结构化输出通道、`constrainedSampling: grammar`、TUI 的 codemode 卡片。

### 2.2 ✅ MCP —— **已实现**（prux 早于 pi 落地，不属于本区间待同步项）

pi 0.99.0 才将 MCP 作为内置扩展加入，prux 的 `src/extensions/mcp{,.rs}` 早已实现，
因此该项标记为**已实现**，不作为待移植功能（余项：`pi.registerMcpServer()` 的扩展注册面，
见第二十四节）。以下仅为形态记录；曾列为可选对照项的
两项（工具渲染标题、OAuth 跨进程刷新串行化）已在第七轮落地（见第十二节）：

- 配置来源：pi = 全局 `mcp.json` + 受信任项目 `.mcp.json` + `pi.registerMcpServer()`；
  prux = `~/.config/mcp`、`~/.agents`、`agent_dir()/extensions/mcp/mcp.json`、
  受信任项目 `.mcp.json` → 配置面已覆盖；❗**但 `pi.registerMcpServer()` 无对应物**
  （第十九轮复核：`Extension` trait 里没有任何 MCP 注册口，扩展无法以会话为作用域注册/注销
  MCP 服务器，只能写配置文件）→ 见第二十四节待移植项。
- CLI：pi = `pi mcp add|remove|list|login|logout`；prux = `prux mcp list|add|update|
  remove|login|logout` → **已覆盖**（多一个 update）。
- 传输：stdio + Streamable HTTP + OAuth → prux `mcp/manager.rs` 已引用
  `StreamableHttpClientTransportConfig`；OAuth 刷新跨进程串行化已落地
  （pi 1d74741e1 → 见第十二节）。
- 工具渲染（见 2.9）：pi 0.99.0 起 MCP 调用标题为 `server/tool`、结果折叠 5 行、
  无自定义渲染器的工具调用显示参数（key=value / 展开逐行）→ 第七轮已对照落地
  （见第十二节）。
- 内置扩展命名 `builtin:mcp` 与 `--no-extensions` 语义（见 2.7）。

### 2.3 🆕 扩展工具编排 API（exposure / outputSchema / nestedCalls 等）—— **子集已落地**（见第十三节）

pi 0.99.0：`exposure`（direct/model-only/codemode/deferred/hidden）、`namespace`、
`annotations`、`outputSchema` + `structuredContent`、`isError` 结果、
`prepareLoadout()`、`ctx.executeTool()` 嵌套调用（事件带 `parentToolCallId`，
记录为调用方结果的有界 `nestedCalls`）。

prux 现状：

- **本轮（第八轮）已落地**：`ToolExposure`（5 值，驱动模型可见面与嵌套可调性，
  迁移期曾把旧的 `deferred_tool_names()` 归一到 `Exposure::Deferred`，该旧接口已删除）、`namespace` 与
  `annotations`（声明 + `/extension` 详情与 `tool_search` 索引消费）、
  `outputSchema`（进 `tool_search` 检索文本）、`ToolResult::structured_content`
  （并进 `details.structuredContent`，MCP 已改走该入口）、
  `ToolExecCtx::execute_tool()` 嵌套调用 + `parentToolCallId` 事件 +
  有界 `nestedCalls` + 嵌套用量并入调用方（计入会话成本）。
- **第十四轮已落地**：`prepareLoadout()` 的等价物（`Extension::prepare_loadout(&ToolLoadout)
  -> ToolLoadoutChanges`，见第十九节）：`compose_tools` 在装配工具表后调用各扩展，把描述覆盖
  写回工具表、把 `hidden_declarations` 交给 agent 循环在**请求期**过滤（工具仍激活、仍可被脚本调）；
  另补 `ToolExecCtx.script_tools`（pi `ExtensionToolContext.tools` 的等价物）与
  `ToolExposure::is_script_callable`（`direct` 需激活、`codemode` 恒可调、`deferred` 需激活、
  `model-only`/`hidden` 不进脚本工具表）。
- **第十五轮已落地**：`isError` 作为**独立于 `text` 的结构化结果字段**
  （`ToolResult.is_error` + `ToolResult::error()`，脚本侧据此判成败，见 20.3）。

### 2.4 🆕 tool_search 内置扩展 —— **已落地**（见第八节）

pi 0.99.0：声明模型未见的工具并在需要时引入。prux 无对应物。
注意 prux 0.87.1 轮已按上游删除了旧 deferred tool loading 管道
（`added_tool_names` 死管道清理），tool_search 是**新机制**，不要与之混淆。

### 2.5 🆕 系统主题（system theme）+ 新色彩体系 —— **已落地**（见第八节、第二十五节）

pi 0.99.0：`system` 主题为默认——从终端上报的前景/背景/16 ANSI 色
（OSC 10/11/4 一次往返 `queryTerminalColors()`）推导配色，明暗切换时重建；
明暗检测顺序 = 背景色 → 终端明暗上报 → `COLORFGBG`；
主题文件支持 `#rgb` / `oklch()` / `okhsl()` 与 `appearance` 字段；
扩展可读 `theme.style()` / `theme.colors` / `theme.appearance`；
内置 dark/light 主题改用 OKHSL 重制；启动 header 改显 logo+版本；
banner 移除 `[Themes]` 段；`TERM=*-direct` 视为 truecolor。

第二十轮补上了其中两项：**索引兜底档**（终端什么都不上报时用 ANSI 0-15 + SGR 2）与
**扩展可读主题色/外观**（`ThemeView`）；**实时明暗切换不做**（用户拍板，理由见 8.6 取舍 2）。

prux 现状：固定 dark/light + `assets/themes` 额外主题（`extra_themes` 扩展同步），
无 OSC 颜色查询、无 oklch/okhsl、无 appearance。若移植，
`utils/terminal_caps.rs` 与 `modes/interactive/theme.rs` 是主要改动面；
TUI 包的 breaking change（`queryTerminalColors()` 取代两个旧查询函数、
删除 `parseOsc11BackgroundColor()`）因 prux 未用过旧函数而无迁移负担。

### 2.6 🆕 Sign in with ChatGPT（openai provider 的 ChatGPT 订阅登录）—— **已落地**（见第九节）

pi 0.99.0：`/login openai` 可用 ChatGPT 订阅走 OpenAI API；稳定 `deviceId`
存全局设置并从 bug report 脱敏；订阅用量上限错误不重试并链接 ChatGPT 用量页。
pi 把原 OpenAI Codex 订阅登录改名 "OpenAI Codex (legacy)" 并**与新的并存**；
prux **不保留旧版**：已删除 `openai-codex`（provider、目录、codex 后端传输、
device-code 登录），订阅登录只在 `openai` provider 上提供（见第九节与三.3）。

### 2.7 🆕 内置扩展的统一命名与禁用机制 —— **部分落地**（见第八节）

pi 0.99.0：内置扩展/工具在错误、诊断、bug report 中统一命名 `builtin:<name>`
（取代 `<inline:name>` / `<builtin:name>`）；slash 命令不再带 `[t]` 标签；
`pi config` 新增 Built-in 段可全局/按项目禁用 `mcp`/`llama.cpp`/`codemode`/
`tool-search`（存为 `-builtin:<name>`）；**`--no-extensions` 现在也禁用内置扩展**，
需 `-e builtin:<name>` 单独加载。

prux 现状：有 `disabled_extensions` 设置与扩展启停过滤
（`settings_manager::write_disabled_extensions`、`extensions.rs` 注册表），
CLI 有 `--no-tools`/`--no-builtin-tools` 等但无 `--no-extensions`；
`defaultTools` 为纯替换语义（项目级覆盖全局，`settings_manager.rs:454`）。
pi 同期新增 `defaultTools` 的 `+name`/`-name` 增删语法
（如 `"defaultTools": ["+read"]`，项目条目叠加在用户设置之上）——
第八轮已落地该语法，但**只作用于内置工具**（见 8.4 的口径说明）。

> **`defaultTools` 的边界（用户拍板，第十九轮明确）**：`defaultTools` 只处理**内置工具**；
> `codemode` / `mcp` / `tool_search` 等属**扩展工具**，不进 `defaultTools`，
> 其启停由 `enabledExtensions`/`disabledExtensions` 与 `/extension` 面板控制。
> 因此 pi 的 `"defaultTools": ["+codemode"]` 在 prux 里映射为「启用 codemode 扩展」，
> 而不是把它当成一个可选工具名——这是**刻意的口径差异**，不是缺功能。

### 2.8 🔧 Virtual models（实验性）—— **已落地**（见第十六节）

pi 0.99.0：扩展经 `pi.registerVirtualModel()` 注册虚拟模型，每次请求路由到
不同物理模型 + thinking level；footer 显示实际路由模型，`/session` 按物理模型
分列成本；`examples/extensions/jev-router.ts` 用 Jev 分类器路由。

prux 第十一轮已落地 **宿主侧全链路**（扩展注册面 `Extension::virtual_models()` +
目录/`/model` 并入 + 每请求路由 + 路由状态落会话 + footer `→ 物理模型` +
路由失败即错误响应）；`/session` 按物理模型分列成本沿用第五轮已有能力；
示例与限制见第十六节。

### 2.9 🔧 Classifier models（Jev 分类器）—— **已落地**（见第十五节）

pi 0.99.0：`Models.classify()` 提供 provider 中立的 `choice`/`score`/`bool` 契约；
内置 TypeSafe `jev-latest`；OpenRouter / Cloudflare Workers AI / Vercel AI Gateway /
OpenCode Zen 继承 Jev 模型；llama.cpp 分类器（prux 不适用）；
`ClassifierResult.usage` 计费。

prux 第十轮已落地 **provider 层**（`provider::classify`，对齐 pi 的
`Models.classify()` 契约：一次性非流式、`bool`↔线上 `noul`、失败不 reject、
`usage` 按目录单价计费）；未做：`cloudflare-workers-ai-system-one`
（prux 无该 provider）、`llama-cpp-classify`（prux 不适用）。
2.8 虚拟模型已在第十一轮落地（第十六节），路由器可以直接用 `provider::classify`；
第十二轮又把 pi 的示例插件 `jev-router.ts` 移植成内置扩展 **`jet`**（第十七节），
分类器因此有了开箱消费者（默认不启用）。详见第十五、十六、十七节。

### 2.10 🆕 图片生成（image generation）—— **已落地**（见第十四节）

pi 0.99.0：`ModelRuntime.generateImages()` + 运行时解析认证；
OpenRouter 图片模型与 chat 模型共凭据，同一上游 ID 可有 chat/image 双条目；
模型目录请求带 `types=chat,image,classifier`。

prux 第九轮已落地 **provider 层**（`provider::generate_images`，对齐 pi 的
`Models.generateImages()` 契约：一次性非流式、失败不 reject、返回 base64 输出 + usage）；
未做：结果落盘/TUI 展示（pi 也归扩展自理）、模型可见的 image 工具（pi 无）、
`onPayload`/`onResponse`/`signal`/`metadata` 选项。详见第十四节。

### 2.11 🆕 `provider_stream_event` 扩展事件 + `/debug-provider` —— **已落地**（见第八节）

pi 0.99.0：在归一化之前观察 provider 原始解析事件（含 assistant 消息不保留的
provider 特有字段）；配套 `/debug-provider` 示例查看器。
prux 事件总线在 `extensions.rs`（boundary/context_with_system/cache_warming_decision
等 dispatch 函数），新增该事件是低风险增量移植。

### 2.12 🆕 HTML 导出：`display: false` 自定义消息的显隐切换 —— **已落地**（见第八节）

pi 0.99.0：自定义消息默认隐藏，导出页 `H` 键或侧栏可切换显示。
prux 有 `core/export_html`，但未见 custom message / `display` 字段支持
→ 该修复依赖 custom message 体系，prux 若不打算支持自定义消息导出则跳过。

### 2.13 🆕 小项 —— **已落地**（见第八节）

- **Claude Sonnet 5.5**（Anthropic，adaptive thinking + 1M 上下文 + 官方计价）：
  prux `models.anthropic.json` 无此条目 → 随目录同步补齐（见第四节）。
- **`fullscreenWheelScrollLines` 设置**（"auto" 模式在本地 macOS 之外的终端
  对快速滚轮事件加速）：prux 滚轮为固定 `WHEEL_STEP=3` + Alt×5
  （`handlers/mouse.rs`）→ 候选小改进。
- **`thinkingLevel` 记录到每条 assistant 消息**（`AssistantMessage.thinkingLevel`，
  agent/ai 0.99.0）：prux `AgentMessage`（`core/provider.rs:277`）无此字段，
  thinking level 仅作为 session 级配置变更条目记录 → 候选小增量，注意会话格式兼容。
- **扩展注册同名 tool/command/flag 顶掉内置扩展时告警**（#10174）：prux 有内置
  扩展集合，可加同款告警。

### 2.14 未实现功能清单（汇总）

以下为 0.87.1→0.99.1 区间 pi 新增、prux 当前**尚未实现**的能力。
（MCP 已实现；2.4 / 2.5 / 2.11 / 2.12 / 2.13 与 2.7 的多数子项已在第三轮落地，
2.6 在第四轮落地，MCP 的两项形态对照在第七轮落地，2.3 的编排子集在第八轮落地，
2.10 图片生成在第九轮落地，2.9 分类器在第十轮落地，2.8 虚拟模型在第十一轮落地，
其中 2.8 的开箱消费者（pi 的示例插件 `jev-router.ts` → 内置扩展 `jet`）在第十二轮落地；
2.1 codemode 与 2.3 的 `prepareLoadout()` 在第十四轮落地，
2.1 的全部余项（bash 1 MiB 结构化输出、`models.*`、`constrainedSampling: grammar`、
TUI 嵌套调用渲染）与 2.3 的 `isError` 在第十五轮落地，
第十六轮又补了本节外的三处缺口（见第二十一节），第十七 / 十八轮再补四处与三项（见第二十二 / 二十三节），
第十九轮订正「刻意不做 / 结构性不适用」判定并新增两行待办（见第二十四节），
第二十轮清掉那两行待办并落地 2.5 的两项取舍（索引兜底档、扩展主题视图）。
详见第八、九、十二、十三、十四、十五、十六、十七、十八、十九、二十、二十一、二十二、二十三、二十四、二十五节。）

**本表已清空**——原先挂着的三行（2.1 余项 / 2.3 余项 / 2.7 余项）状态如下：

| 原项 | 现状 |
|---|---|
| 2.1 余项 | **已落地**（第十五轮，第二十节）：bash/powershell 的 1 MiB 结构化输出、`models.*` 全局、`constrainedSampling: grammar`、TUI 嵌套调用（结果态 + 进行中） |
| 2.3 余项 | **已落地**（第十五轮，20.3）：`ToolResult.is_error` 作为独立于 `text` 的结构化字段，脚本侧据此判成败 |
| 2.7 余项 | **不适用**（第八节 + 2.7 的边界说明）：`defaultTools` 只处理内置工具，扩展工具由扩展启停控制（用户拍板）；`builtin:` 前缀无区分度（prux 无外部扩展加载器）、命令候选无来源标签体系 |

**第十九轮复审后新增的两行待办**——**第二十轮全部落地**（见第二十五节）：

| 待办项 | 现状 |
|---|---|
| macOS Finder 文件粘贴（pi #9999） | ✅**已落地**（25.1）：新增 `read_clipboard_file_paths()`（arboard 的 NSPasteboard file URL / Wayland `text/uri-list`），粘贴优先级改为 文件路径 → 图片 → 文本，bash 模式加 shell 引号、控制字符路径拒绝 |
| `pi.registerMcpServer()` | ✅**已落地**（25.2）：`Extension::mcp_servers()` 声明会话作用域的 MCP 服务器，`mcp.json` 同名条目优先并在 `/mcp` 标出覆盖 |
| 一.13 #9786 X11 剪贴板判型（待验证项） | ✅**已给出结论**（25.1）：prux 走 arboard 的**严格 PNG 解码**，文本不会被当成图片；另加「先看剪贴板声明了哪些目标」的探测，避免请求未声明的图片目标 |

第十六轮又发现并修掉三项：远程目录刷新缺 `types=chat,image,classifier`
（coding-agent 0.99.0 Added）、内置 dark/light 未换成 pi 的 OKHSL 调色板
（coding-agent 0.99.0 Changed）、未知 `type` 条目被当成 chat（ai 0.99.0 Added）——
均已落地，见第二十一节。

**可选对照项**（prux 已有该功能，差在形态而非实现）：
- ~~MCP 工具调用渲染：标题 `server/tool`、结果折叠 5 行、无自定义渲染器时按 key=value 显示参数。~~
  → 第七轮已落地（见第十二节）。
- ~~MCP OAuth refresh 跨进程串行化（pi 1d74741e1）。~~ → 第七轮已落地（见第十二节）。

**非功能项状态**：

- 三.3 的 `--no-extensions` 语义与 `defaultTools` `+/-` 语法已在第三轮落地（见 8.4）；
  原待拍板的 `OpenAI Codex` → `OpenAI Codex (legacy)` 显示名已因第四轮删除该 provider 而不适用。
- 一.12 延伸（第五轮已落地，见第十节）：`/session` 按模型分列成本、
  `compactionSummary`/`branchSummary` 的 usage 在会话投影处被丢。
- 新发现（第六轮已落地，见第十一节）：cache 保温用量落成 `UsageRecord`
  （`type: usage_record`，`cause: cache_warm`），`/session` 的成本合计与分列都已计入；
  为此给 `UsageRecord` 补了 `provider`/`model` 归属字段（旧文件缺省为 `None`，
  无归属的旧记录仍进 `Tools/summaries` 桶）。
- 新发现（第八轮已落地，见 13.1 与 3.1）：目录里本就混装着 `type: image`（57 条）与
  `type: classifier`（10 条）条目，而 `ModelEntry`/`ModelConfig` 此前不读 `type` →
  它们会出现在 `/model` 选择器里，选中后报 `unsupported api type: openrouter-images`。
- 第十二轮新增内置扩展 `jet`（第十七节）：2.8 虚拟模型 + 2.9 分类器的开箱用例，
  即 pi `examples/extensions/jev-router.ts` 的移植（默认不启用，配置在
  `agent_dir()/extensions/jet.json`，无默认值）。
- 第十三轮为**文档轮**（第十八节）：定案 2.1 codemode 的实现形式（内置扩展 `builtin:codemode`
  注册单个 `exposure = model-only` 的 `codemode` 工具，扩展名与工具名 1:1），未改任何代码；
  同时发现 **2.3 余项的 `prepareLoadout()` 是 2.1 的硬前置**（codemode 的动态 description 靠它）。
- 第十四轮（第十九节）落地 codemode 本体与 `prepareLoadout()`：新增
  `src/extensions/codemode{,.rs}`（`sandbox.rs` / `glue.js` / `declarations.rs` /
  `description.rs` / `source.rs` / `store.rs`），核心新增 `Extension::prepare_loadout`
  与 `ToolLoadout`/`ToolLoadoutChanges`、`ToolExecCtx.script_tools`、
  `ToolExposure::is_script_callable`，并把 `Agent.hidden_declarations` 接到请求期过滤。

### 2.15 依赖关系与建议实施顺序（第八轮定案）

#### 共同前置（不做它，2.8 / 2.9 / 2.10 都无从谈起）

**模型目录的 `type` 感知**（第八轮已落地，见 13.1）：

- 目录里已经带着非 chat 条目：`assets/models/` 共 **57 条 `type: image`、10 条
  `type: classifier`**（openrouter / opencode / vercel-ai-gateway）。
- 但 `ModelEntry`（`core/model_resolver.rs`）与 `ModelConfig`（`core/provider.rs`）
  此前都没有 `type` 字段，解析时直接丢掉；`list_models` 也不按 type 过滤。
- 后果：这些图片/分类器模型会出现在 `/model` 选择器里当 chat 模型，而它们的 `api`
  是 `openrouter-images` / `typesafe-system-one`，选中后落到 `stream_chat` 的
  `other =>` 分支，报 `unsupported api type`。
- 因此第 0 步：`type` 进 `ModelEntry`/`ModelConfig` + 列表按 type 过滤 + 按 type 路由。
  顺带修掉上面那个坑，也是 2.9/2.10 的地基（`list_models_of_type` 已备好）。

#### 真实依赖链

| 依赖边 | 依据 |
|---|---|
| **2.3 → 2.1** | codemode 要 `exposure: codemode/deferred/hidden`、`ctx.executeTool()` 的 `parentToolCallId`/`nestedCalls`、`outputSchema`+`structuredContent`。第八轮已把这条链的宿主侧（`ToolExposure` / `ToolExecCtx::execute_tool` / `nestedCalls` 记录）落地，剩余的是 JS 侧 glue |
| **共同前置 → 2.9** | 分类器要 provider 中立的 `classify()` 契约 + `type: classifier` 条目路由 |
| **2.9 → 2.8**（pi 路径；非硬性） | 2.8 的示例 `jev-router` 靠分类器路由；且 2.8 的“按物理模型分列成本”在 prux 已存在（§10.2） |
| **共同前置 → 2.10** | 图片模型解析 + 运行时认证；与 2.9 **互不依赖**，可平行。第九轮已完成 |
| **2.9 ⇢ 2.1**（软依赖） | pi 里 codemode 内可调 `models.classify()`，其 usage 计入 codemode 结果与会话成本（见 2.1 原文）；2.9 已于第十轮落地，2.1 到时可直接用 `provider::classify` |

#### 建议实施顺序

1. **目录 type 感知**（共同前置；顺带修 `/model` 选择器的现存隐患）—— 第八轮已完成。
2. **2.3 子集**（`exposure` / `namespace` / `annotations` / `outputSchema` /
   `ctx.execute_tool` + `nestedCalls`）—— 第八轮已完成；是 2.1 的地基，且本身可独立收窄收益。
3. **2.9 分类器** → 之后 **2.8 虚拟模型**（可复用已有的按模型分列成本）。
   分类器已在第十轮完成（provider 层 `classify`）；虚拟模型在第十一轮完成
   （见第十六节）。
4. **2.10 图片生成**（仅依赖第 0 步，与第 2/3 步无逻辑耦合）—— 第九轮已完成
   （provider 层 `generate_images`；落盘/展示与模型可见工具不做，见第十四节）。
5. ~~**2.1 codemode** 收尾~~ —— **第十四轮已完成**（第十九节）：新增
   `src/extensions/codemode.rs`（`default_enabled() == false`）+ 单个 `ToolExposure::ModelOnly`
   的 `codemode` 工具，并先补上 2.3 余项的 `prepareLoadout()`（codemode 的描述靠它生成）。

改动面冲突提示：2.3 与 2.1 都动 `core/extensions/tools.rs` 与 `agent_session.rs`；
2.9/2.10 已改完 `model_resolver.rs` + `provider.rs`，后续动它们的人请注意
`ModelConfig.output` / `find_model_of_type` / `provider::classify` 已存在；
2.8 已在第十一轮落成（新增 `core/virtual_models.rs`，并改了 `model_resolver.rs` /
`auth.rs` / `agent_loop.rs` / `agent_session.rs`，见第十六节）。并行开发（多个 agent
同时改同一批文件）必打架，建议同一时间只放一个动 `model_resolver.rs` 的活。

---

## 三、移除 / 破坏性变更（0.87.1→0.99.1）

### 3.1 🚫 ai 包：图片模型统一进 `Provider`/`Models` 表面（breaking）—— **不需要同步**

> prux 第九轮已落地图片生成（见 2.10 / 第十四节），但仍无 pi 的
> `ImagesModels` / `createImagesProvider()` 那套独立集合——prux 从一开始就是
> “图片模型进同一份目录、同一份凭据”的形态，因此本项既无被移除的 API，
> 也无迁移负担；仅目录同步脚本需对未知 `type` 字段容错（已在第四节点 4 核验）。
> **不需要同步**。

- 移除 `ImagesModels` 集合及 `createImagesModels()` / `createImagesProvider()` /
  `ImagesProvider` / `openrouterImagesProvider()` / `builtinImagesProviders()` /
  `builtinImagesModels()`；`generateImages()` 只接受 `ImageModel`
  （必带 `type: "image"`）。
- **模型数据 schema 升到 v6**：每条目携带 `type`（chat/image/classifier）；
  操作级目录混合三种类型；同一上游 ID 可按类型多条
  （OpenRouter 图片进 `openrouter.json` 的 `openrouter-images` api group）；
  新增数组式 `models.all.json` / `providers/{id}.all.json`（旧键式 JSON 仍为
  chat-only）；`image-models.generated.ts` 与 `scripts/generate-image-models.ts` 移除。
- 对 prux 的影响面：**不在运行时**（prux 无 `ImagesModels` 那套独立集合，图片模型本就在同一份目录里），
  而在 **目录同步管线**（`scripts/sync-models.py` 从 pi-ai data 目录拉数据）——
  同步脚本与 `assets/models/` 需适配 v6 schema 的 `type` 字段与新文件形态
  （image/classifier 条目已随目录一并引入，见 13.1 与第十四节）。
  `model_refresh.rs::parse_catalog`（pi.dev 远程刷新）需确认能容忍新增 `type` 字段。

### 3.2 🚫 tui 包：终端颜色查询 API 合并（breaking）—— **不需要同步**

> prux 未使用这两个被移除的 API，无迁移负担；仅当移植 2.5 系统主题时按新
> `queryTerminalColors()` 实现。**不需要同步**。

`TUI.queryTerminalColorScheme()` / `queryTerminalBackgroundColor()` 移除，
合并为 `TUI.queryTerminalColors()`（一次往返查 OSC 10/11/4）；
`parseOsc11BackgroundColor()` 移除。

### 3.3 行为/命名变更（非 breaking，但需拍板）

- 🚫 **OpenAI Codex provider 改名 "OpenAI Codex (legacy)"**：pi 保留旧登录并与
  Sign in with ChatGPT 并存；prux **不保留**（用户拍板）——第四轮直接删除
  `openai-codex` provider、目录与订阅登录，改名项因此不适用（见第九节）。
- **`--no-extensions` 语义扩大**（也禁用内置扩展）与内置名 `builtin:<name>`：
  见 2.7；prux 无 `--no-extensions`，若未来加该 flag 应直接采用新语义。
- **`defaultTools` `+`/`-` 增删语法**：见 2.7；prux 现为整组替换。
- 🚫 **durable 包 0.99.0 整包重写**（Storage/Tx/TaskRuntime/Harness 一长串 breaking）：
  prux 无 durable 架构，整体不适用 → **不需要同步**。

---

## 四、模型目录同步项（升级时的机械性工作）

> **状态：已完成**。目录已在 `3cff3ca`（“同步模型文件”）里全量同步（`sync.md` 已记为
> `0.99.1`），且带上了 pi 对 `thinkingLevelMap` / `compat.allowEmptySignature` 的项内修正，
> 因此下面 1–4 无需再做，只需核验（已逐项 grep 确认）。5 的收尾发现并修了 3 处遗留夹具。

pi 0.99.x 相对 0.87.1 的目录变更，随 `scripts/sync-models.py` 全量同步处理：

1. **新增模型**：`gpt-6.1-sol`（openai / openai-codex；azure 排除——prux 无该
   provider）、`claude-sonnet-5.5`（anthropic）、Jev 分类器条目（openrouter /
   vercel-ai-gateway / opencode 等，视 prux 是否引入 2.9 决定）、
   OpenRouter 图片条目（api group `openrouter-images`，prux 暂不需要）。
   → ✅ 已核验：`gpt-6.1-sol` 在 openai 在（`models.openai-codex.json` 已在第四轮
   随 provider 删除）；anthropic 有 `claude-sonnet-5-5`。
   ⚠️ **本条原先误记为「Jev/图片类条目未引入（依赖 2.9/2.10，未做）」**——实际
   同步脚本是把上游条目原样导入的：`assets/models/` 里本就有 **10 条
   `type: classifier`**（openrouter 7 / opencode 2 / vercel-ai-gateway 1）与
   **57 条 `type: image`**（全在 openrouter）。**第八轮（13.1）已实测纠正该结论**：
   这些条目一直都在，只是当时 `type` 被解析丢掉；而 2.9 分类器直到第十轮
   （第十五节）才真正可用。
2. **移除模型**：`kimi-k2.6` 系列在 fireworks / together / opencode-go 上游目录
   中已移除（moonshotai / opencode / openrouter 仍保留）→ 同步后 prux 目录
   自然清理，注意与一.1 的默认模型改指联动。
   → ✅ 已核验：三处已无 kimi-k2.6（fireworks/opencode-go 为 `kimi-k3`，
   together 为 `moonshotai/Kimi-K3`）；moonshotai / moonshotai-cn / opencode /
   openrouter / huggingface / baseten / nvidia / qwen-token-plan* 仍保留。
3. **改名/默认变更**：openai-codex 默认 → `gpt-6.1-sol`；provider 显示名
   "OpenAI Codex (legacy)"（见 3.3）。→ 默认已改（一.2）；显示名改名随第四轮
   删除 `openai-codex` provider 而不再适用。
4. **schema v6 适配**：同步脚本解析 `type` 字段；`models.all.json` 数组式目录
   可暂不采用（旧键式仍发布且 chat-only），但解析端不应因未知字段失败。
   → ✅ 已核验：同步脚本只取 `{api: {id: model}}` 内层值展开，不读未知字段；
   `model_refresh.rs::parse_catalog` 也按 `id` 过滤，容忍额外字段。
5. 同步后跑 `cargo test`（`model_resolver` 的默认模型断言、目录基线断言
   `baseline_models` 均需随目录更新）。
   → ⚠️ 同步提交漏改了 3 处夹具，本次一并修（HEAD 上本就是红的）：
   - `model_resolver::tests::find_model_detects_thinking_capabilities_for_uncatalogued_format`：
     `opencode-go/kimi-k2.6` 已不存在 → 改用 `moonshotai/kimi-k2.6`；
   - `model_resolver::tests::model_entry_carries_catalog_request_limits`：
     `fireworks/deepseek-v4-flash-vision-exp` 已不存在 → 改用
     `accounts/fireworks/routers/kimi-latest`；
   - `anthropic::tests::empty_thinking_signature_replayed_for_compat_models`：
     `fireworks/deepseek-v4-pro` 已不存在 → 改用 `deepseek-v4p1-flash`。

---

## 五、优先级建议（第一轮定案；状态列为第二轮后补）

| 优先级 | 项 | 状态 | 理由 |
|---|---|---|---|
| P0 | 一.1 Kimi K3 默认模型 + 目录清理 | ✔️ | 默认模型指向已移除的上游模型，新会话直接坏 |
| P0 | 一.2 gpt-6.1-sol + codex 默认 | ✔️ | 0.99.1 的核心变更，目录同步即可带大半 |
| P0 | 四 目录全量同步（schema v6 适配） | ✔️（已同步，并修 3 处遗留夹具） | 一.1/一.2 的前置 |
| P1 | 一.3 Responses output_index 报错 | ✔️ | 静默混排工具调用是正确性问题 |
| P1 | 一.4 qwen3.8-flash thinking 空签名 | ✅（已满足，补回归） | prux 有对应 provider，实际可触发 |
| P1 | 一.5 `@` 补全包装符 | ✔️ | 高频交互路径，改动小 |
| P2 | 一.6/一.7 计费修复（Vercel 1h cache、Fast tier） | ✔️ | 成本低估，非功能性 |
| P2 | 一.8 OAuth 错误处理/端口兜底 | ✔️ | 登录失败路径 |
| P2 | 一.9 Copilot Opus 5.5 thinking levels | ✅（目录已带修正值，补回归） | 越界提供不支持档位 |
| P3 | 一.10 主题尊重终端能力覆盖 | ✔️ | 256 色终端下的错误色彩序列 |
| P3 | 一.11 主题残留旧色 / 流式 CPU | ✔️（残留色结构性不存在；CPU 采纳 2 项） | 长会话体验 |
| P3 | 一.12 嵌套调用 usage 计入会话成本 | ✔️（统计口径） | 底栏成本漏算子代理开销 |
| P2 | 二.2 MCP | ✅ 已实现；两项形态对照已在第七轮落地 | prux 早于 pi 落地；渲染形态与刷新串行化见第十二节 |
| P2 | 二.11 provider_stream_event | 🆕 已落地 | 低风险增量 |
| P3 | 二.4 tool_search、二.12 HTML 导出显隐、二.13 小项 | 🆕 已落地 | 中等特性/增量 |
| P3 | 二.5 系统主题、二.7 defaultTools 语法/`--no-extensions` | 🆕 已落地（2.7 余项不适用） | 体验/一致性改进 |
| P3 | 二.6 ChatGPT 登录（含删除 openai-codex） | 🆕 已落地 | 体验改进 |
| P3 | 一.12 延伸：摘要 usage 进会话投影 + `/session` 按模型分列成本 | 🆕 已落地（第五轮） | 底栏/面板成本口径 |
| P3 | 缓存保温用量（`usage_record`）计入合计与分列 | 🆕 已落地（第六轮） | 成本口径完整性 |
| P1 | 目录 `type` 感知（chat/image/classifier 分流 + 修选择器隐患） | 🆕 已落地（第八轮，共同前置） | 不做它，2.8/2.9/2.10 无处落；且非 chat 条目现会被当 chat 模型 |
| P1 | 二.3 扩展工具编排子集（exposure/outputSchema/`ctx.execute_tool`+nestedCalls） | 🆕 已落地（第八轮） | 2.1 的地基，独立可收窄收益 |
| P3 | 二.1 codemode、二.3 余项（`prepareLoadout`） | 🆕 已落地（第十四轮，第十九节） | 形态 = 内置扩展 `codemode.rs` + 单个 `model-only` 工具；`prepareLoadout` 等价物同时补齐（描述覆盖 + 请求期隐藏声明） |
| P3 | 二.8 虚拟模型 | 🆕 已落地（第十一轮，第十六节） | 扩展注册面 + 每请求路由 + 路由状态/事件；“按物理模型分列成本”复用第五轮 |
| P3 | 二.8 的开箱消费者：内置扩展 `jet`（jev-router 移植 + `/jet` 命令） | 🆕 已落地（第十二轮，第十七节） | 2.8 + 2.9 的端到端用例；默认不启用、无默认值，配置有效才注册 `jet/auto` |
| P3 | 二.9 分类器（provider 层 `classify`） | 🆕 已落地（第十轮） | 与 pi `Models.classify()` 同契约；只做 `typesafe-system-one` |
| P3 | 二.10 图片生成（provider 层 `generate_images`） | 🆕 已落地（第九轮） | 与 pi `Models.generateImages()` 同契约；落盘/展示不做 |
| 不做 | 三.1/3.2 的 pi 侧 breaking（运行时面）、durable、llama.cpp、Mistral、macOS/Kitty/RPC | ❌/🚫 | 结构性不适用或不需要同步 |

> 验证基线：改动前后 `cargo test` + `cargo clippy --all-targets` + `cargo fmt --check`。
> 目录同步注意既有 flake：`resolve_provider_model_falls_back_to_default` /
> `resume_selector_ctrl_jk_navigation_and_paste` 在改动前 HEAD 上即偶发失败。
> 第二轮实测：改前 HEAD `cargo test --lib` 为 **1954 passed / 3 failed**（均为目录同步
> 遗漏的陈旧夹具）；改后 **全绿**（lib 1974 passed / 0 failed，外加全部集成测试）。

---

## 六、实施记录（第二轮：代码落地）

首轮审计中标记为 🔧/⚠️ 的 12 项已全部处理完毕。按“实际结论”分三类：

### A. 真正改了代码（8 项）

| 项 | 改动 | 新增/更新的验证 |
|---|---|---|
| 一.1 | `model_resolver::default_model_id`：fireworks/opencode-go/together → `kimi-k3` | `default_model_follows_pi_table` 新增三项断言 |
| 一.2 | 同上：openai-codex → `gpt-6.1-sol` | 同上 + `codex_catalog_excludes_retired_gpt_5_4_models` 的旧断言更新 |
| 一.3 | `responses.rs`：`output_item.added` 重用未定稿索引即报错；流末仍有未定稿工具调用报错 | `reused_output_index_ends_with_error` / `unfinished_tool_call_ends_with_error` / `finished_tool_call_passes` |
| 一.5 | `suggest.rs`：`PATH_WRAPPERS` + `strip_leading_wrappers` + `is_at_token_start`；`last_word` 起始索引跳过包装符 | `last_word_strips_leading_wrappers` / `at_after_wrapper_is_token_start` / `at_file_suggestions_trigger_after_wrapper` |
| 一.6 | `anthropic.rs::handle_anthropic_message_delta`：补读 `cache_creation.ephemeral_1h_input_tokens` | `message_delta_reports_1h_cache_write` |
| 一.7 | `usage.rs`：`service_tier_cost_multiplier` / `apply_service_tier_cost`；`responses.rs` 读响应 `service_tier` 并在计费后缩放 | `service_tier_multiplier_matches_pi` / `fast_service_tier_applies_multiplier` |
| 一.8 | `oauth.rs::poll_callback` 带描述返回 `Err`；`LoginKind::AuthorizeCode.server` 改 `Option`（anthropic/codex 端口占用退化为粘贴 URL） | `callback_rejects_missing_params`（错误重定向分支改写）/ `occupied_callback_port_falls_back_to_manual_paste` |
| 一.10 | `theme.rs`：`fg/bg/color` 按终端能力分派真彩/256 色（`rgb_to_ansi256` 对齐 pi）；`terminal_caps.rs` 认 `TERM=*-direct` | `color_output_honors_terminal_capability` / `rgb_to_ansi256_matches_pi` |

### B. 核对后确认「已满足/无需改代码」（2 项，补回归测试锁行为）

- **一.4**：目录已带 `compat.allowEmptySignature`，`anthropic.rs::compat_allow_empty_signature`
  已在重放侧保留空签名（与 pi 同语义）→ 改测试夹具 `empty_thinking_signature_replayed_for_compat_models`
  为现存条目，并新增 `qwen38_flash_empty_signature_replayed`。
- **一.9**：`claude-opus-5.5` 目录已带 pi 的修正 `thinkingLevelMap`（off/minimal=null，
  low..max 显式映射），`supported_thinking_levels()` 推出 low..max → 新增
  `opus_5_5_offers_low_through_max`。

### C. 核对后按 prux 架构调整了实施方案（2 项）

- **一.11**：
  - 旧色残留：**结构性不存在**（`SysSpan` 存主题键、每帧解析；`msg_cache` 以
    `theme.render_fingerprint()` 为失效键）→ 不写多余的重建逻辑，只补测试。
  - 流式 CPU：采纳「跨帧派生输入缓存」（`App::derived_tool_results` /
    `derived_edit_map` + `messages_fingerprint`，见 `render/history.rs`）与
    `sanitize_output` 快路径；不采纳 pi 的 footer/bash 结果级缓存
    （prux 已逐消息缓存，口径不同）。测试：
    `derived_render_inputs_cached_until_messages_change` /
    `derived_render_inputs_survive_theme_change`。
- **一.12**：pi 的 `nestedCalls` 记录体系未移植（属第二节 2.3），只补「统计口径」：
  新增 `footer::scan_usage` 统一三处底栏的 usage 汇总，把带 usage 的 `toolResult`
  计入会话成本（prux 的 `afterToolCall` → toolResult `usage` → 落盘链路已在）→ 测试
  `tool_result_usage_counts_toward_session_totals`。

### D. 顺手修的目录同步遗留（第四节 5）

三处测试夹具引用已被上游删除的模型（改前 HEAD 本就失败）：
`find_model_detects_thinking_capabilities_for_uncatalogued_format`（opencode-go kimi-k2.6
→ moonshotai kimi-k2.6）、`model_entry_carries_catalog_request_limits`（fireworks
vision 条目 → `accounts/fireworks/routers/kimi-latest`）、
`empty_thinking_signature_replayed_for_compat_models`（fireworks deepseek-v4-pro
→ deepseek-v4p1-flash）。

### 未落地的候选（留给下一轮）

未实现功能以 **2.14 汇总清单**为准，本轮全部未动：2.1 codemode、2.3 工具编排 API、
2.4 tool_search、2.5 系统主题、2.6 ChatGPT 登录、2.7 builtin 命名/`defaultTools` 语法、
2.8/2.9 虚拟模型/分类器、2.10 图片生成、2.11 `provider_stream_event`、
2.12 HTML 导出显隐、2.13 小项；以及 2.14 中的 MCP 可选对照项。
（2.2 MCP 本身已实现，不计入未落地项。）
- 三.3 的显示名/语义拍板项：`OpenAI Codex` → `OpenAI Codex (legacy)`、
  `--no-extensions` 语义、`defaultTools` `+/-` 语法。
- 一.12 的延伸：`/session` 按模型分列成本（`getUsageCostBreakdown`）、
  摘要类消息（`compactionSummary`/`branchSummary`）usage 在会话投影处被丢。

> 一.12 的延伸已在第五轮落地（见第十节）；三.3 的显示名项随第四轮删除 provider 而不适用；
> 2.14 中的 MCP 两项可选对照项已在第七轮落地（见第十二节）；
> 本节的其余未落地项仍以 2.14 汇总清单为准。

---

## 七、第二轮验证命令与结果

```bash
cargo fmt --check          # 通过（先 cargo fmt 修过格式）
cargo clippy --all-targets # 仅剩既有 warning（examples/*、settings_manager.rs、tasks/widget.rs）
cargo test                 # 全通过：lib 1974 passed / 0 failed / 1 ignored；集成测试全绿
```

改动文件（含文档共 19 个，代码 +1085/−138）：
`core/model_resolver.rs`、`core/oauth.rs`、`core/oauth/{anthropic,openai_codex,openrouter}.rs`、
`core/provider/{anthropic,responses,usage}.rs`、`extensions/footer{,.rs}`（`footer.rs` +
`footer/{normal,rich}.rs`）、`modes/interactive/app.rs`、
`modes/interactive/handlers/suggest.rs`、`modes/interactive/oauth_flow.rs`、
`modes/interactive/render/history.rs`、`modes/interactive/theme.rs`、
`utils/display.rs`、`utils/terminal_caps.rs`，以及 `migrations/migration-v0.99.1.md`（本文档）。
未提交 commit（按项目规则：未显式要求不自动提交）。

---

## 八、第三轮：2.4 / 2.5 / 2.7 / 2.11 / 2.12 / 2.13 落地记录

按用户指定实现 2.4、2.5、2.7、2.11、2.12、2.13；其余（2.1、2.3、2.6、2.8–2.10）不动。

### 8.1 二.11 `provider_stream_event` + `/debug-provider`（完整移植）

- `ExtensionHook::ProviderStreamEvent` + `Extension::provider_stream_event(&Value)`；
  事件形状 `{type, provider, api, model, data}`，`data` 为该条 SSE 载荷的原始 JSON。
- 四个 provider 的流解析点各接一次（completions / anthropic / responses / google），
  **一次流只查一次** `has_provider_stream_event_handlers()`：无扩展订阅时零开销。
- 新增内置扩展 `src/extensions/debug_provider.rs`（`/debug-provider [on|off|dump]`，
  `default_enabled() == false` 按需开启）：缓冲原始事件 → 每轮 assistant 消息结束时
  落一条 `customType=debug-provider-events` 的会话条目 + 渲染一张卡片，`dump` 打印完整 JSON。

### 8.2 二.12 HTML 导出显隐切换（移植 + prux 形状适配）

- `assets/export-html/template.{js,css}`：`hook-message-hidden` + `body.show-hidden-messages`
  + 头部按钮 + `H` 键（逐条对齐 pi #10020）；深链接跳到隐藏条目前会先自动显示。
- prux 侧多认一种条目形状：`type == "custom"` 且 `data.content` 为字符串时按自定义消息导出，
  `data.display === false` 视为隐藏；状态类 custom 条目（plan-mode 等，无 `content`）仍不导出。

### 8.3 二.13 小项（三项全部落地）

| 项 | 实现 | 备注 |
|---|---|---|
| `fullscreenWheelScrollLines` | `utils/wheel.rs`（`WheelScrollAccelerator`，逐值对齐 pi `wheel-scroll.ts`）+ `/settings` 档位 `auto/1/2/3/5/10` | **行为变化**：默认 `auto` → 孤立一格滚 1 行、快速连滚最多 6 行（原来固定 3 行）；停靠面板与消息区现在用同一行数 |
| `thinkingLevel` 落每条 assistant 消息 | `AgentMessage.thinking_level`（serde `skip_serializing_if`，旧会话兼容），agent 循环在收响应时写入 | prux 无逐消息的树视图 UI，该字段目前只作记录/会话格式对齐 |
| 同名覆盖告警 | `extensions::name_collisions()`，启动与 `/reload` 时提示 | prux 扩展全为内置，等价于「内置扩展之间重名检测」 |

### 8.4 二.7 builtin 命名 / 禁用机制（部分落地）

- **已落地**：`defaultTools` 的 `+name`/`-name` 增删语法（`merge_default_tools` +
  `resolve_default_tools`，逐值对齐 pi）；`--no-extensions` 禁用**含内置**的全部扩展
  （`set_all_extensions_disabled`，注册后解析 flag 故同时立即生效；`/extension` 面板仍可临时开启）。
- **不适用**：`builtin:<name>` 前缀（prux 无外部扩展加载器，全部扩展都是内置，前缀无区分度）、
  命令候选的 `[t]` 来源标签（prux 命令候选没有来源标签体系）、`pi config` 的 Built-in 段
  （`/extension` 面板 + `disabledExtensions` 已覆盖同样语义，键名保持裸名以免破坏既有配置）。
- **扩展工具不进 `defaultTools`**（用户拍板）：`defaultTools` 的作用域是**内置工具**；
  `codemode`/`mcp`/`tool_search`/`jet` 等属**扩展工具**，由 `enabledExtensions`/
  `disabledExtensions` 与 `/extension` 面板控制。因此 pi 的 `"defaultTools": ["+codemode"]`
  等价于 prux 里「启用 `codemode` 扩展」，**不是**缺一个可选工具名。
  同理，`ToolSelection::Default` 的“扩展工具全并入”是刻意行为
  （`--tools/-t` 的 `Allowlist` 也只要求扩展已启用），与 pi 的 per-tool 激活模型不同口径。
- **口径变化（对齐 pi）**：项目层 `defaultTools: []` 现在**不再**清空全局列表（空列表不含
  `+`/`-` 条目，按「无修改」叠加）。全局层 `defaultTools: []` 仍是「无内置工具」。

### 8.5 二.4 tool_search（移植 + 最小 deferred 机制）

pi 的 `tool_search` 建立在 2.3 的 `exposure: deferred` 之上；本轮只取**最小切片**：

- 延迟工具由工具自身声明：`ExtensionTool::with_exposure(ToolExposure::Deferred)`（本轮为最小切片
  曾用扩展级的 `Extension::deferred_tool_names()`，后已删除，口径统一到 exposure）。
- `compose_tools` 统一口径 `visible_extension_tools()`：未激活的延迟工具既不进系统提示也不进工具表；
  `find_extension_tool`（执行解析）不受影响。
- 激活集合 + 脏标志：`activate_deferred_tools` / `take_deferred_activation_dirty`，
  agent 循环在工具执行后据此 `rebuild_tools()`，因此**下一轮模型调用**即可见（对齐 pi）。
- `src/extensions/tool_search.rs`：BM25（k1=1.2、b=0.75）+ pi 的 tokenize/stem/schema 文本，
  `/settings` 无需配置；默认关闭（无延迟工具时它只占一个工具位）。

### 8.6 二.5 系统主题（完整移植 + 三处取舍）

**移植范围**（与 pi 逐值一致，测试用 pi 自身实现在 node 下导出的黄金值锁定）：

- `utils/color.rs`：Oklab/OKLCH/OKHSL ↔ sRGB 全套数学 + `#rgb`/`#rrggbb`/`oklch()`/`okhsl()`
  解析（含 OKLCH 越界时保持色相的二分降彩度）。
- `utils/terminal_colors.rs`：OSC 10/11/4 一次往返查询（尾部 DA1 作为结束标记）+ 纯函数解析器；
  非 TTY / `PRUX_TERMINAL_COLORS=0` / 非 unix 不查询；查询前后自管 raw 模式，
  故必须在 `setup_terminal()` **之前**调用（`run_interactive` 首行 `prime_terminal_colors()`）。
- `modes/interactive/system_theme.rs`：pi `system-theme.ts` 的完整推导（色族表 / 15 条明度曲线 /
  57 条规则 / 依赖序求解 / 中灰背景的松弛二分 / 正文 token 用终端前景 / WCAG 4.5:1 收尾）。
  表格由 pi 运行时导出生成，非手工转录。
- `theme.rs`：主题 JSON 支持 `oklch()`/`okhsl()`/`#rgb`/`ansi:<n>`（数字）、`appearance` 字段
  （未声明时从主题颜色推导）；`Theme::appearance()`；`system` 主题名进入 `/theme` 列表；
  **默认主题改为 `system`**（终端不上报配色时用索引兜底档，见第二十五节）。
- 启动 banner 的 `[Themes]` 段已移除（`--verbose` 启动报告）。启动 header 本就是 logo+版本，
  已满足 pi 0.99 的同款改动。

**取舍（与 pi 不同）**：

1. ~~不做 ANSI 索引兜底档~~ → **第二十轮已实现**（见第二十五节）：终端什么都没上报时
   输出 `ansi:<槽位>`（彩色 token 用色族槽位）+ 面板透明 + 中性非正文 token 走 SGR 2，
   不再回落内置 dark/light。
2. **不做实时明暗切换通知**（pi 启用 mode 2031 并监听 `CSI ? 997;n`）——**用户第二十轮拍板不做**。
   第十九轮复核时判为“技术上可行（只是成本）”，第二十轮实测后发现成本比原先估计更硬：
   crossterm 0.29 的解析器对**不认识且以 `?` 开头的 CSI 不是丢弃而是留在缓冲里**
   （`parse_csi` 的 `?` 分支只处理 `?u`/`?c`，其余返回 `Ok(None)` 一直等后续字节），
   于是 `CSI ? 997;n` 会吞掉之后的所有按键，直到用户恰好按下 `c`/`u` 才整段清空；
   而 crossterm 直接从 fd 0 读、没有可注入的解析口，要接这个报告只能自持真实终端的读端
   （把 stdin 换成自建 pty 再由我们转发）。该实现已写就后按用户指示**整体移除**（第二十五节），
   现状与 pi 的差异如实记为：外观只在启动时判定一次（顺序：上报背景 → COLORFGBG）。
3. ~~不向扩展暴露 `theme.style()/colors/appearance`~~ → **第二十轮已实现**（见第二十五节）：
   `core/theme_view.rs` 的 `ThemeView`（TUI 每帧发布解析后的语义色 + 外观），
   HTML 导出也改用它（修掉 `system` 主题导出成内嵌 dark 的老问题）。

### 8.7 验证

```bash
cargo test        # lib 2018 passed / 0 failed / 1 ignored；集成测试全绿
cargo clippy --all-targets   # 仅剩既有 warning（examples/*、settings_manager、tasks/widget）
cargo fmt --check # 通过
```

额外验证：真实 pty 驱动 `prux` 并回放 OSC 回复 —— 上报 `bg=#1e1e1e` + 16 色时渲染出
system 主题的 `accent=#d379d1`；不回复时渲染内置 dark 的 `accent=#8abeb7`（回落正确）。

改动文件：`utils/{color,terminal_colors,wheel}.rs`（新增）、`modes/interactive/system_theme.rs`（新增）、
`extensions/{debug_provider,tool_search}.rs`（新增）、`core/{extensions,extensions/*,agent_loop,agent_session,provider/*,settings_manager,export_html/export}.rs`、
`modes/interactive/{theme,interactive,render/startup,handlers/*,settings_selector}.rs`、
`cli/args.rs`、`main.rs`、`assets/export-html/template.{js,css}`、`Cargo.toml`（nix 加 `poll` feature）、
`tests/*`。未提交 commit（按项目规则：未显式要求不自动提交）。

---

## 九、第四轮：Sign in with ChatGPT（二.6）+ 删除 openai-codex

按用户指定实现二.6，并**不保留** pi 的旧版——`openai-codex`（provider + 订阅登录）
整体删除，订阅登录只在 `openai` provider 上提供。

### 9.1 删除：openai-codex provider 与旧 OpenAI 登录

旧登录（codex client `app_EMoamEEZ73f0CkXaXp7hrann` + `chatgpt.com/backend-api` +
device-code/browser）是 prux 里唯一需要 ChatGPT 账号的路径，provider 也只支持 OAuth，
因此“删登录 = 删 provider”。删除清单：

| 类别 | 删除内容 |
|---|---|
| 代码 | `src/core/provider/codex.rs`（codex 后端 SSE/WS 传输）、`src/core/oauth/openai_codex.rs`（browser + device-code 登录） |
| 目录 | `assets/models/models.openai-codex.json` |
| 注册/分发 | `login_registry` 的 openai-codex 项、`SUPPORTED_PROVIDERS`/`baseline_models`/`default_model_id` 条目、`provider::stream_chat` 的 `"openai-codex-responses"` 分支、`oauth::{start_login,exchange_login,refresh_oauth}` 分支 |
| 类型 | `OAuthCredential::chatgpt_account_id`（仅 codex 用）、`start_login_with(provider, method)`（method 仅 codex 用）、`stream_chat(..., transport)`（唯一消费者是 codex，全部调用点同步收敛） |
| 文案 | `render/panel.rs` 的 openai-codex 登录说明分支、`assets/docs/{custom-provider,extensions,http-proxy}.md` 中的 codex 协议/传输提法 |

模型未丢：`gpt-6.1-sol` 等条目本就在 `models.openai.json`（`api: openai-responses`），
订阅用户改在 `openai` provider 上使用；回归测试改为断言
`baseline_models("openai-codex")` 为空、`default_model_id("openai-codex")` 为 `None`
且不在 `SUPPORTED_PROVIDERS`（`openai_catalog_keeps_retired_codex_models`）。

### 9.2 新增：`oauth/openai_chatgpt.rs`（对齐 pi `auth/oauth/openai-chatgpt.ts`）

- 常量逐值对齐：`authorize=https://auth.openai.com/api/accounts/authorize`、
  `token=.../api/accounts/oauth/token`、`resource=https://api.openai.com/v1`、
  `redirect_uri=http://127.0.0.1:1455/auth/callback`、
  `scope="openid profile email offline_access resource.invoke chatgpt.tokens.use.direct"`、
  过期提前量 **3 分钟**（pi `EXPIRY_MARGIN_MS`，与其它 provider 的 5 分钟不同）。
- 授权 URL 带 `client_id=dynamic_agent_client`、`agent_name_hint`、`ext_agent_host_id=urn:uuid:<deviceId>`、
  `resource`、`nonce`、PKCE(S256)、随机 `state`。
- **动态 client id**：OpenAI 在回调里回传实际签发的 client_id，刷新 token 必须回传。
  为此打通了一条新链路：`CodeResult.client_id`（回调查询参数）→
  `AuthorizationInput.client_id`（手动粘贴解析）→ `finish(..., client_id)` →
  `oauth::exchange_login(..., client_id)` → 凭据 `clientId`（auth.json）→ `refresh_token`。
- 凭据校验：`access/refresh/expires_in` 必填，`scope` 必含 `chatgpt.tokens.use.direct`，
  交换时还要求响应含 `id_token`（不透传使用，与 pi 一致）。
- 端口 1455（与 Codex CLI 共用）被占用时 `server=None`，退化为纯粘贴 redirect URL。

### 9.3 新增：稳定 `deviceId` 设置

`settings_manager::{read_settings_device_id, get_or_create_device_id}`：首次被登录流程
需要时生成 UUID v4 并写入**全局** settings.json（惰性写入，与 pi 同语义）；
`bug_report::redacted_settings_file` 直接移除 `deviceId` 键（pi 亦从报告中剔除）。

### 9.4 请求路径与错误文案适配

- `responses.rs`：新增 `is_chatgpt_sign_in(model)`（provider=openai ∧
  baseUrl=`https://api.openai.com/v1` ∧ key 非空且不以 `sk-` 开头，对齐 pi 的判别）；
  命中时**不发** `max_output_tokens` 与 `temperature`。为此把请求体构造抽成
  `build_request_body()` 便于单测；请求体其它字段（`store:false`、reasoning、tools）不变。
- 错误文案：`provider_error_body()` 在错误体含
  `subscription_sharing_usage_limit_exceeded` 时追加
  `Check your ChatGPT usage: https://chatgpt.com/settings/usage`。
- `agent_session::is_retryable_assistant_error` 的 `NON_RETRYABLE` 增加同一错误码
  （订阅共享用量上限需等数小时，退避重试无意义）。

### 9.5 与 pi 的偏差（已知且刻意）

1. **不保留 openai-codex**（pi 改名 legacy 并行保留）——用户拍板；prux 侧因此没有
   `chatgpt.com/backend-api` 通路，也不再有 `chatgpt_account_id` 请求头。
2. **不新增 `PI_OAUTH_CALLBACK_HOST` 等价开关**：prux 回调服务器固定绑 `127.0.0.1`
   （与 anthropic/openrouter 一致），pi 允许用环境变量改 host。
3. **粘贴输入不校验 host/path**：pi 要求粘贴的 URL 以 `redirect_uri` 开头；prux 只要求
   能解析出 `code` 与（openai 必需的）`client_id`，非法输入仍给出粘贴提示。
4. `agent_name_hint` 用 `"Prux"`（pi 为 `"Pi"`）。

### 9.6 验证

```bash
cargo test                    # lib 2021 passed / 0 failed / 1 ignored；全部集成测试通过
cargo clippy --all-targets    # 仅剩既有 warning（examples/*、settings_manager.rs:260、tasks/widget.rs）
cargo fmt --check             # 通过
```

新增/改写的测试：`oauth::openai_chatgpt::{credential_requires_direct_token_scope,
client_id_is_required_from_callback, agent_host_id_requires_uuid, auth_url_contains_expected_params}`、
`oauth::tests::{parse_manual_input_variants（新增 client_id 用例）}`、
`responses::{chatgpt_sign_in_omits_rejected_request_fields, chatgpt_usage_limit_error_links_usage_page}`、
`agent_session`（订阅用量上限不重试）、`bug_report`（deviceId 不入报告）、
`login_registry::{subscription_supports_account_login, all_registry_providers_support_api_key}`、
`model_resolver::openai_catalog_keeps_retired_codex_models`。

改动文件：新增 `src/core/oauth/openai_chatgpt.rs`；删除
`src/core/provider/codex.rs`、`src/core/oauth/openai_codex.rs`、`assets/models/models.openai-codex.json`；
修改 `core/{oauth,oauth/*,auth,login_registry,model_resolver,agent_session,agent_loop,bug_report,settings_manager,provider,provider/*}.rs`、
`modes/interactive/{oauth_flow,handlers/auth,render/panel}.rs`、`extensions/proxy/server.rs`、
`assets/docs/*.md`。未提交 commit（按项目规则：未显式要求不自动提交）。

---

## 十、第五轮：一.12 的两项遗留（摘要 usage 进投影 + `/session` 成本分列）

按用户指定实现 1.12 遗留的两项；其余（2.1、2.3、2.8–2.10 与 2.14 的可选对照项）不动。

### 10.1 摘要 usage 进会话投影（修 footer 拿不到的问题）

根因不是「统计口径」，而是**投影丢字段**：`session_manager::summary_message` 只带 role 与文本，
条目上的 `usage` 在投影成模型上下文消息时被丢掉；而 `ctx.messages`（= `Agent::messages`
= `Session::build_context_messages()`）是 footer 唯一的数据源 → 摘要生成的开销在底栏永远为 0。

- `summary_message(role, summary, usage: Option<&Usage>)` 现在把 `Entry::Compaction` /
  `Entry::BranchSummary` 的 `usage` 挂到投影消息上（`AgentMessage.usage`）。
- `extensions/footer.rs`：删掉第二轮留下的死桩 `msg_cache_usage()`（永远返回 `None`）——
  usage 现在随消息进来，`scan_usage` 的 `compactionSummary` / `branchSummary` 分支才真正生效。
- `/session` 侧不重复累加：`accumulate_message_usage` 对这两个 role 提前返回
  （合计仍由 `accumulate_session_usage` 从条目算，覆盖全部 lane / 全部压缩）。
- 安全性：投影消息只用于「渲染 + 统计 + 转成 API 载荷」，四个 provider 的转换函数
  都只读 role/content（`convert_summary_message` 等），`usage` 不会外发；
  `compaction::get_last_assistant_usage_info` 只认 `role == "assistant"`，
  故上下文占用估算不会被摘要 usage 带偏。

### 10.2 `/session` 按模型分列成本（移植 `getUsageCostBreakdown`）

- `worker::usage_cost_breakdown(messages, entries)`（对齐 pi `core/usage-totals.ts`）：
  assistant 消息按 `provider/responseModel||model` 归集（网关 `auto` 之类会解析出具体模型）；
  `toolResult`（嵌套调用）与 `compaction`/`branch_summary`（摘要生成）没有模型归属，
  统一进 `Tools/summaries` 桶；过滤 `cost > 0 || tokens > 0`，按成本降序。
- 数据源与 `session_stats` 的成本合计**逐一对齐**（消息 + 会话条目的摘要），
  所以分列之和恒等于 `Cost Total`（pi 注释里的 “reconciles with the session total”）。
- `SessionStats` 新增 `cost_breakdown: Vec<CostBreakdownEntry>`；渲染在 `on_session_stats`：
  合计行之后逐条 `  <key>: $x.xxx (N tokens)`，行末换行数与 pi 一致
  （有分列时合计行单换行、最后一条双换行，保持 `Cache Warming` 块前的空行）。
- 与 pi 同口径的省略规则：**只有一条且正好是当前选中模型**（`current_provider/current_model`）时
  不列分列行——否则它只是把合计重写一遍。
- token 格式化复用 footer 的 `format_tokens`（`1.2k` / `12k` / `1.2M`），与 pi `formatTokens` 一致。

### 10.3 与 pi 的偏差

1. pi 的分列完全基于 `sessionManager.getEntries()`（含其它 lane 的条目）；prux 的
   assistant / toolResult 部分基于 `a.messages`（活动分支的投影），摘要部分基于条目——
   与 prux 既有的 `/session` 合计口径一致（本轮不改合计语义），代价是分支会话下
   与 pi 的绝对值可能不同，但**内部自洽**（分列之和 == 合计）。
2. pi 的 `type: "usage"` 条目（缓存保温用量）prux 对应 `UsageRecord`：第六轮已补齐
   `provider`/`model` 归属字段并计入合计与分列（见第十一节）。

### 10.4 验证

```bash
cargo test                    # lib 2028 passed / 0 failed / 1 ignored；全部集成测试通过
cargo clippy --all-targets    # 仅剩既有 warning（均在 examples/bot/*）
cargo fmt --check             # 通过
```

新增测试（6 个）：

| 测试 | 锁住的行为 |
|---|---|
| `session_manager::projected_summaries_carry_entry_usage` | 投影的 compactionSummary 带条目 usage，重开会话后仍在 |
| `footer::normal::summary_usage_counts_toward_session_totals` | 摘要 usage 计入底栏 ↑in/↓out/$cost |
| `worker::cost_breakdown_groups_by_provider_model_and_tools_bucket` | 分组键、`responseModel` 优先、Tools 桶合并、成本降序、token 合计 |
| `worker::cost_breakdown_drops_zero_usage_entries` | 零成本零 token 的条目不列出 |
| `handlers::events::session_stats_lists_cost_breakdown_by_model` | 渲染文本（含与 `Cache Warming` 块的换行关系） |
| `handlers::events::session_stats_omits_single_breakdown_for_selected_model` | 单条命中当前模型省略 / 不命中仍列出 |

改动文件：`core/session_manager.rs`、`extensions/footer.rs`、`extensions/footer/normal.rs`（测试）、
`modes/interactive/app.rs`、`modes/interactive/worker.rs`、`modes/interactive/handlers/events.rs`。
未提交 commit（按项目规则：未显式要求不自动提交）。

---

## 十一、第六轮：缓存保温用量（`UsageRecord`）计入 `/session` 合计与分列

起因是第五轮记下的「新发现」：保温请求不经会话回合（不落 assistant 消息），
它的用量只以 `LaneRecord::UsageRecord`（`type: "usage_record"`, `cause: "cache_warm"`）
存在，而 `/session` 的合计与分列都只认 `compaction`/`branch_summary` → 这部分成本
在会话成本里完全不可见（只有 Cache Warming 块的 `Warmed: N time(s), $X` 单列显示）。
pi 侧的对应物是 `type: "usage"` 条目（`appendUsage(kind, provider, model, usage, note?)`），
`getSessionStats` 与 `getUsageCostBreakdown` 都把它算进去、并按 `provider/model` 归集。

### 11.1 给用量记录补归属字段（`core/session_v4.rs`）

`LaneRecord::UsageRecord` 新增 `provider` / `model`（`Option<String>` + `skip_serializing_if`），
与 pi `UsageEntry.provider/model` 对齐——没有归属就只能进 `Tools/summaries` 桶，
看不出是哪个模型烧的钱。

- 旧会话文件里已有（或将来由别处写入）的用量记录没有这两个字段：`Option` 缺省为 `None`，
  解析不受影响；序列化时也不写出空字段 → 旧文件的字节形态不变。
- 写入侧 `append_cache_warm_usage(usage, provider, model)`（`core/session_manager.rs`）
  由 worker 的 `drain_cache_warm` 传入 `o.provider` 与 `o.response_model ?? o.model_id`
  （与 pi `message.provider` / `responseModel ?? model` 同口径）。

### 11.2 计入合计与分列（`modes/interactive/worker.rs`）

- `accumulate_session_usage` 改为接收 `&[Value]`（可单测），识别集合加上 `usage_record`
  → 保温开销进入 `/session` 的 Cost Total / token 合计。
- `usage_cost_breakdown` 的条目分支：`usage_record` 带 `provider`+`model` 时按
  `<provider>/<model>` 归集（与 assistant 消息同一把 key，因此两者会合并成一行），
  否则进 `Tools/summaries`。
- 不变量仍成立：合计与分列读的是同一批条目 → **分列之和 == Cost Total**。

### 11.3 行为变化（需知悉）

- `/session` 的 `Cost Total` / `Input` / `Output` 现在包含缓存保温开销（此前不含）。
  与 pi 一致（pi 的合计含 `usage` 条目）。
- 保温成本因此会同时出现在 `Cost` 块与 `Cache Warming` 块的 `Warmed: N time(s), $X` 行；
  pi 同样如此（Cache Warming 块显示 `Refresh cost`，合计也含它），不是重复计费。
- `SessionState.stats`（会话选择器/归约统计）本来就统计 `usage_record`，本次未动。

### 11.4 验证

```bash
cargo test                    # lib 2030 passed / 0 failed / 1 ignored；全部集成测试通过
cargo clippy --all-targets    # 仅剩既有 warning（settings_manager.rs、examples/bot/*）
cargo fmt --check             # 通过
```

新增/更新的测试：

| 测试 | 锁住的行为 |
|---|---|
| `session_v4::usage_record_without_attribution_still_loads` | 旧 `usage_record` 行（无 provider/model）仍能解析，缺省为 `None` |
| `session_manager::cache_warm_usage_is_recorded_and_counted_in_stats`（更新） | 记录带上 `provider`/`model`，`get_entries()` 里可见 |
| `worker::cost_breakdown_includes_cache_warm_usage_and_reconciles_with_totals` | 保温按模型归集/合并、无归属进 Tools 桶、合计 == 分列之和 |

> 既有 flake（与本轮无关，实测）：`core::agent_session::tests::subagent_background_spawn_*`、
> `extensions::subagent::{manager,schedule}::tests::*`、`extensions::subagent::prompt::tests::
> compact_and_custom_modes`、`handlers::commands::tests::extension_detail_lines_display_commands`
> 在**全量并行**跑时偶发失败（这些测试共用全局 subagent 状态 / 读 `PRUX_AGENT_DIR`）。
> 证据：改动前的 HEAD 上 13 次全量跑也失败 1 次；把本轮新增测试全部 `--skip` 掉后
> 5 次里仍失败 2 次 → 属既有竞态，与本次改动无关（单跑均通过）。

改动文件：`core/session_v4.rs`、`core/session_manager.rs`、`modes/interactive/worker.rs`、
`migrations/migration-v0.99.1.md`。未提交 commit（按项目规则：未显式要求不自动提交）。

---

## 十二、第七轮：MCP 两项形态对照（渲染 + OAuth 刷新跨进程串行化）

按用户指定落地 2.14「可选对照项」里的两项（2.2 MCP 的形态差异）；其余
（2.1、2.3、2.8–2.10）不动。

### 12.1 MCP 调用渲染（对齐 pi 0.99.1）

prux 的 MCP 是**单个 `mcp` 代理工具**（pi 是每工具一个 `mcp__server__tool`），
因此按 prux 的工具形状对齐 pi 的三种渲染形态，不改工具注册模型：

- **`server/tool` 标题**：`action=call` 时标题为 `server/name`，参数取该 MCP 工具自身的
  `arguments`；其余 action 标题为 `mcp <action>`，参数为除 `action` 之外的实参。
- **无自定义调用头的工具显示参数**（pi `formatToolCallWithArgs` →
  `messages.rs::generic_call_header`）：折叠时同一行 `key=value`（字符串带引号，
  超过 `COLLAPSED_ARGS_CHARS`=100 字符截断成 `...`），展开时每个参数一行
  `  key: value`（字符串原样、其余按 JSON 缩进，续行再缩进 4 空格）。
  「有自定义调用头」的集合 = `bash` + `CUSTOM_CALL_HEADER_TOOLS`
  （`read/write/edit/ls/grep/find/powershell`，保持原有 path/pattern 形态）；
  扩展工具（`mcp`/`tasks`/`subagent`/`goal`/`web_access` 等）因此都改成显示参数。
- **结果折叠 5 行**：`mcp` 工具的 toolResult 预览从 20 行改为 `MCP_PREVIEW_LINES`=5
  （其余工具不变），提示行仍为 `... (N more lines, ctrl+o/alt+click to expand)`。

与 pi 的偏差：pi 的标题 `server/tool` 来自每工具注册的 `label`，prux 是从
`action/server/name` 参数现推；`formatToolCallWithArgs` 里 pi 用 `JSON.stringify`
而 prux 用 `serde_json::to_string`（同为紧凑 JSON，字符串都带引号）。

### 12.2 MCP OAuth refresh 跨进程串行化（移植 pi 1d74741e1）

会轮换 refresh token 的服务器被两个 prux 进程同时刷新时，后写者会覆盖先写者的新
token，把授权用废。pi 的做法是「每个服务器一把刷新锁，锁从读 token 一直持到新 token
落盘，锁内重读发现已被别人换掉就直接用新 token」。

prux 的刷新由 rmcp `AuthorizationManager` 内部发起，prux 只控制 `CredentialStore`
与 OAuth HTTP 客户端，因此实现落在两处：

- `utils/file_lock.rs`：新增 `LockWait`（总预算 / 重试间隔 / 陈旧阈值）与
  `FileLock::acquire_waiting`，供「持锁超过一次读-改-写」的调用方覆盖等待参数。
- `extensions/mcp/oauth.rs`：
  - `RefreshCoordinator`：按服务器 URL 哈希出刷新锁文件
    （`oauth.json.refresh-<hash>`，`FileLock` 再追加 `.lock`），
    `acquire()` 用 `spawn_blocking` 阻塞等待（不占运行时工作线程），
    `stored_tokens()` 锁内重读凭据文件。
  - `OAuthHttpExecutor`：等价于 rmcp 内置 reqwest 客户端（跟随/不跟随重定向两个
    客户端、30s 超时、1 MiB 响应体上限），使 `AuthorizationManager` 走可拦截的 HTTP 层。
  - `RefreshSerializedOAuthHttpClient`：只拦截 `grant_type=refresh_token` 的 token 请求。
    取锁 → 锁内重读：refresh token 已被换掉则用落盘的新 token **合成 200 响应**
    （不再发这次刷新，避免用废掉的新 token）→ 否则持锁发出请求（15s 超时）；
    成功时**保持锁到 `FileCredentialStore::save()` 落盘后再释放**，失败/超时立即释放。
  - `RefreshCoordinator` 由 `FileCredentialStore`（释放）与 HTTP 客户端（取锁）共享；
    不刷新 token 的调用（`has_credentials` / `logout`）构造的存储不带协调器。

参数逐值对齐 pi：等待 25s、重试 100ms、锁陈旧 20s、单次刷新请求 15s。

与 pi 的偏差与取舍：
1. pi 比较的是 access token（`state.tokens.access_token !== staleToken`），prux 比较
   refresh token（请求体里就是它）——不轮换 refresh token 的服务器会多发一次无害刷新。
2. pi 另有「关闭连接时等待进行中的刷新」；prux 的连接生命周期由 manager 缓存管，
   不做该等待（刷新结果由 `save()` 落盘，未落盘即进程退出与 pi 同样丢 token）。
3. pi 用 `proper-lockfile` 的心跳/接管；prux 复用 `FileLock`（unix `flock` 随进程退出
   自动释放；非 unix 按锁文件年龄 20s 接管）。

### 12.3 验证

```bash
cargo test        # lib 2037 passed / 0 failed / 1 ignored；全部集成测试通过
cargo clippy --all-targets   # 仅剩既有 warning（settings_manager.rs、examples/bot/*）
cargo fmt --check # 通过
```

新增测试：

| 测试 | 锁住的行为 |
|---|---|
| `render::messages::tests::mcp_call_header_shows_server_slash_tool_with_args` | `action=call` 标题 `server/tool` + `key=value` 参数 |
| `render::messages::tests::mcp_call_header_expanded_lists_one_arg_per_line` | 展开态逐参数一行 |
| `render::messages::tests::mcp_non_call_action_shows_action_and_key_value_args` | 非 call 动作标题 `mcp <action>` |
| `render::messages::tests::mcp_result_preview_folds_to_five_lines` | MCP 结果折叠 5 行 + 提示 |
| `render::messages::tests::generic_tool_header_shows_key_value_args` | 无自定义头的工具折叠/展开两种形态 |
| `render::messages::tests::collapsed_args_cut_at_hundred_chars` | 参数对 100 字符截断 |
| `mcp::oauth::tests::refresh_is_serialized_across_managers_and_reuses_new_tokens` | 两个 manager 并发刷新 → 假服务器只收到一次 refresh grant、零 invalid_grant、后来者拿到同一新 token、落盘为最新凭据 |

（该 OAuth 测试手工反向验证过：把锁内重读去掉后测试即失败——后来者会用已被轮换掉的
refresh token 再刷一次。）

改动文件：`utils/file_lock.rs`、`extensions/mcp/oauth.rs`、
`modes/interactive/render/messages.rs`、`migrations/migration-v0.99.1.md`。
未提交 commit（按项目规则：未显式要求不自动提交）。

---

## 十三、第八轮：目录 type 感知（共同前置）+ 2.3 扩展工具编排子集

按 2.15 的建议顺序实施第 1 步与第 2 步；其余（2.1、2.3 余项、2.8–2.10）不动。

### 13.1 第 1 步：目录 `type` 感知（修现存隐患 + 2.9/2.10 的共同前置）

- `core/provider.rs` 新增 `ModelType`（`Chat`/`Image`/`Classifier`，serde `lowercase`，
  缺省 `Chat`）+ `from_catalog` / `as_str` / `is_chat`；
  `ModelConfig.model_type` 带 `#[serde(default, skip_serializing_if = "ModelType::is_chat")]`
  （旧目录 / 动态缓存 / 会话内快照缺该字段时仍是 chat）。
- `ModelEntry.model_type`（`core/model_resolver.rs`）：`find_model` 解析目录 `type`，
  `model_config_from_entry` 透传进 `ModelConfig`。
- **列表按 type 过滤**：`list_models` 现在是 `list_models_of_type(provider, Chat)` 的别名
  （`/model` 选择器、子代理目录、`--list-models`、proxy `/v1/models` 共用的口径），
  新增 `list_models_of_type` 作为 2.9/2.10 的取用入口。
- **按 type 拦住请求**：`stream_chat` 在分发前检查 `model_type != Chat`，
  返回 `model <id> is not a chat model (type: image, provider: openrouter)`，
  取代原先的 `unsupported api type: openrouter-images`（更快指出“选错了模型类型”）。
- `default_model_for` 的兜底从“合并视图第一个条目”改为“第一个 **chat** 条目”
  （openrouter 目录里 image 条目排在最前，旧逻辑会把 FLUX 当默认模型）。

修复的现存缺陷（第八轮审计新发现）：`assets/models/` 里本就混装 57 条 `type: image`、
10 条 `type: classifier`（openrouter / opencode / vercel-ai-gateway）。此前 `type`
被解析丢掉，这些条目会出现在 `/model` 选择器里当 chat 模型，选中即报
`unsupported api type`。现在它们不再出现在任何 chat 列表里。

新增测试：`model_lists_are_filtered_by_catalog_type`（chat 列表不含 image/classifier，
`list_models_of_type` 取得到，类型带进 `ModelEntry`/`ModelConfig`）、
`missing_catalog_type_defaults_to_chat`、`default_model_is_always_chat`（遍历全部
`SUPPORTED_PROVIDERS` 的默认模型）、`provider::mock_tests::non_chat_model_is_rejected_before_dispatch`
（不联网，两个非 chat 类型都被拒且 usage 为空）。

### 13.2 第 2 步：2.3 扩展工具编排子集

#### 暴露方式（`exposure`）

`core/extensions/tools.rs` 新增 [`ToolExposure`]（`Direct` 默认 / `ModelOnly` /
`Codemode` / `Deferred` / `Hidden`）与 `ToolAnnotations`（`read_only`/`destructive`/
`idempotent`）。`ExtensionTool` 新增 `exposure` / `namespace` / `annotations` /
`output_schema` 四个字段（`simple()` 给默认值，全仓 26 处 `ExtensionTool` 字面量构造点
与 22 处 `ToolExecCtx` 字面量构造点同步补字段）。

- **可见性**：`ExtensionTool::exposure` 是唯一口径（迁移期曾有 `effective_exposure()` 把旧的
  `deferred_tool_names()` 归一到 `Deferred`，二者均已删除），`agent_session::visible_extension_tools`
  据此过滤系统提示与工具表：`codemode`/`hidden` 永不进模型可见面，`deferred` 需
  `tool_search` 激活（`deferred_tools()` / `unactivated_deferred_tools()` 共用同一口径）。
- **嵌套可调性**：`ModelOnly` 拒绝其它工具调用，`Deferred` 需已激活，其余允许（见下）。
- **消费点（prux 侧无权限系统）**：`/extension` 详情行显示 `namespace/tool (exposure, 注解)`
  （`extensions/registry.rs`）；`tool_search` 的 BM25 文档文本新增 `namespace` 与
  `output_schema` 的字段名（按输出字段名也能搜到工具）。

#### 结构化输出（`outputSchema` + `structuredContent`）

`ToolResult.structured_content` 是唯一的声明入口，核心在
`FinalizedToolCall::from_tool_result` 把它并进 `details.structuredContent`
（落盘 / 渲染 / 事件三处同形，与 MCP 原有形状一致），MCP 已改走该入口
（不再自己往 `details` 塞键）。

#### 嵌套工具调用（`ctx.execute_tool`）

`ToolExecCtx` 新增 `execute_tool`（`Arc<dyn Fn>` 字段）+ `parent_tool_call_id` +
`nested_calls`（每层一份账本）；同名 inherent 方法 `ctx.execute_tool(name, args)`
供扩展直接调用。宿主侧 `make_exec_ctx` 拆出 `build_exec_ctx`，把 `execute_tool`
重进嵌套上下文（扩展工具在嵌套里也能继续编排）。

语义（`agent_session::run_nested_tool`）：

- **解析**：与模型路径同一套 `ToolIndex`（注入 > 内置 > 扩展注册表），参数按工具
  schema 校验；`ToolExposure` 门控嵌套可调性，**递归深度上限 4 层**，`parent_abort`
  为真时拒绝。
- **事件**：每次嵌套调用发一对 `tool_execution_start` / `tool_execution_end`，
  带 `parentToolCallId`（顶层调用不带）与 `result.details.nestedCall` 记录。
- **记录**（有界 32 条，含**被拒**的调用，超出只计 `nestedCallsDropped`）：
  `{toolCallId, toolName, args, isError, text, durationMs, usage?, structuredContent?, nestedCalls?}`；
  `args`/`text` 超过 4 KiB 时截断（args 退化为 `{"_truncated": ...}`）；更深层的嵌套
  记在各自子记录里（树形，不是平铺）。
- **用量**：嵌套（含更深层）的 usage 逐项求和并入调用方 `ToolResult.usage` →
  经既有的 toolResult usage 口径计入会话成本（一.12）；`details.nestedCalls` 与
  usage 在 `apply_after_tool_call` **之前**合并（扩展替换 details 时不会被静默吞掉）。
- **顶层计数器**：底栏工具计数（`footer/rich.rs`）、假死检测的“工具在跑”
  （`hang_detect.rs`）、子代理 widget 的活动与工具数（`subagent/manager.rs`）都跳过
  带 `parentToolCallId` 的事件（新增 `core::extensions::is_nested_call_event`）——
  否则一次编排会被算成 N 次工具使用。

新增测试（`core/agent_session.rs`）：

| 测试 | 锁住的行为 |
|---|---|
| `nested_tool_calls_are_recorded_counted_and_evented` | 端到端：内置（bash）与 hidden 工具都可嵌套调；`model-only` 被拒（文案含 `exposure: model-only`）、未知工具报 not found；四者都进 `details.nestedCalls`；`structured_content` 落 `details.structuredContent`；嵌套 usage 合计进调用方 usage；嵌套事件带 `parentToolCallId`、顶层只有 1 条不带 |
| `nested_tool_calls_are_bounded_and_truncated` | 35 次嵌套 → 记录 32 条 + `dropped=3`；超长 text 截断、超长 args 退化为 `_truncated`；账本取走后清空 |
| `nested_tool_calls_respect_depth_limit_and_abort` | 深度上限与取消信号都拒绝，且被拒调用同样记账 |
| `exposure_controls_model_visible_tool_surface` | `direct`/`model-only` 进工具表与提示；`hidden`/`codemode` 都不进；`deferred` 需激活后进（并出现在 `unactivated_deferred_tools`） |
| `structured_content_lands_in_details` | 有/无 `details` 时都并成 `details.structuredContent` |
| `is_nested_call_event_requires_parent_id` | 只有非空 `parentToolCallId` 才算嵌套事件（`null` 不算） |
| `extensions::tests::extension_detail_lines_show_namespace_exposure_and_annotations` | 详情行 `ns/plain_tool (read-only)`、`inner_tool (hidden)` |
| `tool_search::tests::namespace_and_output_schema_are_searchable` | 按输出 schema 字段名与 namespace 都能命中 |

### 13.3 与 pi 的偏差（已知且刻意）

1. **嵌套调用不走 `beforeToolCall` / `afterToolCall` 钩子**：它们是“调用方工具的内部
   实现细节”，钩子里拿不到 assistant 消息与上下文；参数 schema 校验照做。
2. ~~**`exposure = codemode` 当前等价 `Hidden`**：prux 还没有 codemode 沙箱，
   `ToolExposure::Codemode` 的语义（“只对沙箱可见”）已声明，但消费方要等 2.1。~~
   → **已消解**（第十四轮 codemode 落地后由 `ToolExposure::is_script_callable` 区分：
   `Codemode => true`、`Hidden => false`；第十九轮复核确认该条已过期）。
3. **枚举语义按 prux 自定义口径**（本轮无 pi 源码可逐值对照，已按行为与文档写明）：
   `direct`/`model-only` 模型可见、`deferred` 需激活、`codemode`/`hidden` 模型不可见；
   嵌套可调性 = 除 `model-only` 与未激活 `deferred` 外都允许。
4. **未移植**：`prepareLoadout()`（prux 是启动时静态注册 + `rebuild_tools`）、
   `isError` 作为独立结果字段（prux 用 `ToolResult`/`ToolError` 表达）。
5. **`namespace` / `annotations` 无权限语义**：prux 无权限系统，它们只用于展示
   （`/extension` 详情）与检索（`namespace` 进 BM25 文本）。

### 13.4 验证

```bash
cargo test                    # lib 2050 passed / 0 failed / 1 ignored；全部集成测试通过
cargo clippy --all-targets    # 仅剩既有 warning（settings_manager.rs、examples/bot/*）
cargo fmt --check             # 通过
```

> 既有 flake（与本轮无关，实测）：`modes::interactive::handlers::commands::tests::
> plan_todos_commands_follow_extension_lifecycle` 在**全量并行**跑时偶发失败
> （plan-mode 进程级内存态 + settings 文件竞态）。证据：把本轮改动 `git stash` 后
> 在 HEAD 上连跑 5 次也失败 1 次；单跑该用例始终通过。

改动文件：`core/{provider,model_resolver,agent_session,extensions,extensions/tools,extensions/registry}.rs`、
`core/tools/index.rs`、`core/provider/mock_tests.rs`、`extensions.rs`（`util` 改 `pub(crate)`）、
`extensions/{mcp,goal,tasks,subagent,web_access,tool_search,hang_detect}.rs`、
`extensions/footer/rich.rs`、`extensions/subagent/{manager,workflow/*}.rs`、
`tests/{subagent,web_access,event_messages}.rs`、`Cargo` 无变化、本文档。
未提交 commit（按项目规则：未显式要求不自动提交）。

---

## 十四、第九轮：2.10 图片生成（provider 层一次性 `generate_images`）

按用户拍板：**只做 provider 层 + 扩展可直接调用**（严格对齐 pi），
不注册模型可见工具（pi 无）、不落盘/展示（pi 也归调用方自理）、不做 CLI/TUI 入口。
参考实现：pi `packages/ai/src/api/openrouter-images.ts`、`Models.generateImages()`
（`packages/ai/src/models.ts`）、`AssistantImages` / `ImagesContext`（`packages/ai/src/types.ts`）。

### 14.1 协议层（新增 `src/core/provider/images.rs`）

- `ImageContent`：`Text { text }` / `Image { data, mimeType }`，JSON 形状逐字段对齐 pi
  的 `TextContent` / `ImageContent`（同一枚举同时用于输入与输出，与 pi 的
  `ImagesInputContent` / `ImagesOutputContent` 复用同一对类型一致）。
- `AssistantImages`：`api` / `provider` / `model` / `output` / `responseId` / `usage` /
  `stopReason` / `errorMessage` / `timestamp`（camelCase，可序列化给扩展）。
- `generate_openrouter_images()`：
  - 请求体 `{model, messages:[{role:"user", content:[…]}], stream:false, modalities}`
    ——`modalities` 为 `["image","text"]` 当且仅当目录 `output` 含 `text`，否则 `["image"]`
    （对齐 pi `buildParams`）；文本块过 `sanitize_surrogates`，图片块内联为
    `data:<mime>;base64,<data>` 的 `image_url`。
  - 发送：`build_client()`（含出站代理）+ `bearer_auth` + `provider_extra_headers`
    （models.json headers / authHeader）+ 单请求总超时 `MAX_TIMEOUT_MS`（300s），
    重试走既有 `send_with_retry`（`DEFAULT_MAX_RETRIES` + 模型级 `max_retry_delay_ms`）。
  - 解析：`choices[0].message.content`（非空 → 文本块）+ `message.images[].image_url`
    （字符串或 `{url}`）中**只接受 `data:` URL**，其余跳过（对齐 pi 的正则语义）。
  - usage：`prompt_tokens` / `completion_tokens` / `prompt_tokens_details.{cached_tokens,
    cache_write_tokens}`，其中 cache write 从 cached 里扣除（对齐 pi `parseUsage`），
    费用交给既有 `usage::compute_cost`（因此自动享受 prux 的 tier / 1h cache 计费）。

### 14.2 入口与目录（`provider.rs` / `model_resolver.rs`）

- `provider::generate_images(model, input) -> AssistantImages`：**失败永不抛错**，
  非 `type: image` 模型、未实现的 api、缺凭据、HTTP 非 2xx、解析失败一律返回
  `stop_reason == "error"` + `error_message`（对齐 pi `Models.generateImages()` 的
  “never reject”契约）。目前只分发 `openrouter-images`（pi 唯一的内置图片实现）。
- `ModelConfig.output: Vec<String>`：承载目录 `output` 模态，供 `modalities` 使用
  （serde 缺省 + 空则不写，旧会话快照 / 动态缓存不受影响）。
- `ModelEntry.output` + `model_config_from_entry` 透传。
- `find_model_of_type(provider, id, kind)`（对应 pi `models.getModelOfType(type, …)`）：
  `find_model` 重构为 `find_model_impl(provider, id, Option<ModelType>)` 的两层壳，
  不限类型时行为与错误文案**逐字不变**；类型不匹配时错误带上 `type: <kind>`。
  必要性：目录里 `google/gemini-3-pro-image`、`openrouter/auto(-beta)` 同时有 chat 与
  image 条目（`api` 分别是 `openai-completions` 与 `openrouter-images`），
  `find_model` 只保证“首个匹配”，按类型取条目必须用它。
- 顺带更新了 `list_models_of_type` / `ModelType` 的过时注释（原文写着“图片生成未移植”）。

### 14.3 文档与示例

- `assets/docs/models.md`：「支持的 API」加 `openrouter-images` + 「图片生成」小节
  （`type: image` 条目不进 `/model` 选择器；`models.json` 自定义条目也可声明
  `type` / `output`，原样透传）；Model 字段表加 `type` / `output` 两行。
- `assets/docs/sdk.md`：新增「图片生成」小节（取条目 → 组装 → 调用 → 读输出），
  与 `examples/image_gen.rs` 对应。
- `examples/image_gen.rs`（新增）：`cargo run --example image_gen -- --prompt "…"`；
  列可用图片模型、按类型取条目、调用、打印 usage，并把 base64 **由示例自己**落盘
  （演示“落盘是调用方的事”这一层；核心库不写盘）。

### 14.4 与 pi 的偏差（已知且刻意）

1. **不做模型可见工具、不落盘、不在 TUI 展示**（用户拍板）：pi 也没有内置 image 工具，
   `generateImages` 的落盘/展示同样归扩展；prux 的 TUI 只能渲染 `[image]` 占位符。
2. **无 `onPayload` / `onResponse` / `signal` / `timeoutMs` / `maxRetries` / `metadata`
   选项**：prux 的 provider 层其余入口（`stream_chat` / `simple_completion`）也没有这些；
   取消/超时由单请求 `MAX_TIMEOUT_MS` 与重试策略约束。
3. **不带 `x-session-affinity`**：pi 的图片请求也不带会话亲和头（图片无提示缓存语义）。
4. **`provider` 字段只支持 `openrouter-images`**：pi 内置同样只有这一个，
   其余走扩展自注册（prux 目前无扩展注册图片 api 的入口，需要时再加）。
5. **未改 `/model` 选择器与 proxy `/v1/models`**：它们走 `list_models`（chat-only），
   图片模型依旧不可被选为 chat 模型（这是 13.1 的行为，本轮不动）。

### 14.5 验证

```bash
cargo test                    # lib 2059 passed / 0 failed / 1 ignored；全部集成测试通过
cargo clippy --all-targets    # 仅剩既有 warning（settings_manager.rs、examples/bot/*）
cargo fmt --check             # 通过
```

新增测试（8 个）：

| 测试 | 锁住的行为 |
|---|---|
| `provider::images::tests::openrouter_images_parses_output_and_usage` | 端到端：请求体形状（`modalities` / `stream` / content 块 / Bearer）、文本 + 两种 `image_url` 形状的 `data:` 图片、非 data URL 跳过、usage 拆分与按目录单价计费、`stopReason=stop` |
| `provider::images::tests::image_only_model_requests_image_modality` | 只出图模型发 `["image"]` 且输入图片按 data URL 内联 |
| `provider::images::tests::provider_error_returns_error_result` | 非 2xx（400）→ error 结果带状态码与响应体 |
| `provider::images::tests::missing_api_key_returns_error_result` | 缺凭据 → error 结果，不发请求 |
| `provider::images::tests::non_image_model_and_unknown_api_are_rejected_before_request` | chat 模型 / 未实现 api 都在发请求前被拒（不联网） |
| `provider::images::tests::data_url_parsing_rejects_non_data_urls` | data URL 解析边界（非 data、空 mime、空 data、缺 `;base64,`） |
| `provider::images::tests::usage_splits_cache_write_from_cached` | `cache_write_tokens` 从 `cached_tokens` 扣除 |
| `model_resolver::tests::find_model_of_type_selects_the_entry_of_that_type` | 同 ID 的 chat/image 双条目按类型各取一条（api/output 不同）、类型不匹配报错带 `type:`、`find_model` 带上 `output` |

改动文件：新增 `src/core/provider/images.rs`、`examples/image_gen.rs`；
修改 `core/provider.rs`（`output` 字段 + `generate_images` 入口 + `mod images`）、
`core/model_resolver.rs`（`find_model_of_type` + `output` 透传）、
`assets/docs/{models,sdk}.md`、14 处测试/夹具的 `ModelConfig`/`ModelEntry` 字面量补 `output`、
`migrations/migration-v0.99.1.md`（本文档）。未提交 commit（按项目规则：未显式要求不自动提交）。

---

## 十五、第十轮：2.9 分类器（provider 层一次性 `classify`）

按用户指定实现 2.9，参考 pi `packages/ai/src/api/{system-one-shared,typesafe-system-one}.ts`
与 `Models.classify()`（`packages/ai/src/models.ts`）、`Classifier*` 类型
（`packages/ai/src/types.ts`）。**只做 provider 层 + 扩展可直接调用**（与第九轮图片生成同口径）：
不注册模型可见工具（pi 无）、不接 agent 循环、不做 CLI/TUI 入口。

### 15.1 协议层（新增 `src/core/provider/classifier.rs`）

- 公开类型（serde camelCase，可直接给扩展）：
  - `ClassifierContext { state, questions }`：`state` 是待分类的 JSON 对象，
    `questions` 是 问题 id → 问题；答案按同一批 id 返回。
  - `ClassifierQuestion`：`Choice { instructions, criteria: 选项 id→说明 }` /
    `Score { instructions, criteria: [刻度文案] }` / `Bool { instructions, criteria: BoolCriteria }`。
  - `ClassifierAnswer`：`Choice { choice, probabilities, confidence }` /
    `Score { score, confidence }` / `Bool { probability }`。
  - `ClassifierResult { api, provider, model, answers, usage?, stopReason, errorMessage?, timestamp }`
    ——字段与 pi `ClassifierResult` 同名同形。
- **`bool` ↔ 线上 `noul`**：请求把 `Bool` 问题的 `type` 写成 `noul`（判据原样下发），
  响应里认 `{type:"noul", noul:<p>}` 并还原成 `ClassifierAnswer::Bool { probability }`
  ——问题与答案两侧都转，对齐 pi `wireRequest` / `parseAnswers`。
- 请求：`POST <base_url>/systemone`（`base_url` 去尾斜杠），体为
  `{ model, state, questions }`（无服务商信封）；`Bearer <apiKey>` +
  `provider_extra_headers`（models.json headers / authHeader）；单请求总超时
  `MAX_TIMEOUT_MS`（300s），重试走既有 `send_with_retry`（`DEFAULT_MAX_RETRIES` + 模型级
  `max_retry_delay_ms`）。
- 解析：`answers` 按请求里的问题逐个校验（缺答案 / 类型对不上 / 字段不是有限数字都算失败），
  响应里多出来的键忽略；错误文案逐字对齐 pi 的 `{LABEL} did not return ...`
  （`LABEL = "System One API"`）。
- usage：`{ input_tokens, output_tokens }`，**两个字段都没有 → 视为服务未报告用量（None）**；
  字段不是正数按 0 计（对齐 pi `tokenCount`）；费用交既有 `usage::compute_cost` 按目录单价算
  （因此自动享受 tier / 1h cache 计费口径）。**用量在解析答案之前落进结果**——
  答案格式不对的请求同样已计费，返回的是带 `usage` 的 error 结果而不是丢掉用量。

### 15.2 入口与目录（`provider.rs` / `model_resolver.rs`）

- `provider::classify(model, context) -> ClassifierResult`：**失败永不抛错**——
  非 `type: classifier` 模型、未实现的 api、缺凭据、HTTP 非 2xx、解析失败、
  答案格式不对一律返回 `stop_reason == "error"` + `error_message`
  （对齐 pi `Models.classify()` 的 "never reject" 契约）。目前只分发
  `typesafe-system-one`（pi 另有 `cloudflare-workers-ai-system-one` 与
  `llama-cpp-classify`，见 15.4）。
- 目录早已就位（10 条 `type: classifier`：openrouter 7 / opencode 2 /
  vercel-ai-gateway 1，`api` 全是 `typesafe-system-one`），无需改目录；
  `find_model_of_type(provider, id, ModelType::Classifier)` 与
  `list_models_of_type(provider, ModelType::Classifier)` 是取用入口
  （第八轮已备好，本轮只更新了「classifier 尚无消费者」的过时注释）。

### 15.3 文档与示例

- `assets/docs/models.md`：「支持的 API」加 `typesafe-system-one` + 「分类器」小节
  （`type: classifier` 条目不进 `/model` 选择器；`models.json` 自定义条目也可声明
  `type` + 该 api 走同一条路径）；`type` 字段说明补上分类器指向。
- `assets/docs/sdk.md`：新增「分类器」小节（取条目 → 组装问题 → 调用 → 读答案），
  与 `examples/classify.rs` 对应。
- `examples/classify.rs`（新增）：`cargo run --example classify -- --text "…"`；
  列可用分类器模型、三道题（choice/score/bool）一起问、打印答案与 usage。

### 15.4 与 pi 的偏差（已知且刻意）

1. **只做 `typesafe-system-one`**：`cloudflare-workers-ai-system-one` 需要
   `cloudflare-workers-ai` provider（prux 无该 provider 与目录），`llama-cpp-classify`
   需要 llama.cpp（prux 排除，见文档头）；两者都不做。
2. **无 `signal` / 取消**：prux provider 层其余入口（`generate_images` / `stream_chat` /
   `simple_completion`）也没有取消参数，因此 `stop_reason` 只有 `stop` / `error`，
   不产生 pi 的 `"aborted"`。
3. **无 `temperature` / `onPayload` / `onResponse` / `maxRetries` 选项**：System One
   本来就没有 temperature 字段（pi 侧也是忽略），其余选项 prux 的 provider 层统一没有。
4. **错误文案口径沿用 prux**：HTTP 失败是 `provider returned <status>: <body>`，
   pi 是 `System One API error (<status>): <body>`；答案格式类失败两边文案一致。
5. **消费者**：第十二轮已落地内置扩展 `jet`（第十七节），用 `provider::classify` 判定
   题面复杂度来选规划模型；codemode（2.1，未做）仍是另一潜在调用方。分类器用量仍不入账。
6. `questions` / `answers` / `probabilities` 用 `BTreeMap`（键按字典序序列化），
   pi 用 JS 对象（插入序）——仅键序不同，语义无关。

### 15.5 验证

```bash
cargo test                    # lib 2073 passed / 0 failed / 1 ignored；全部集成测试通过
cargo clippy --all-targets    # 仅剩既有 warning（settings_manager.rs、tasks/widget.rs、examples/bot/*）
cargo fmt --check             # 通过
```

> 既有 flake（与本轮无关，实测）：首轮全量并行跑时
> `core::agent_session::tests::subagent_background_spawn_notifies_and_persists_child_session`
> 失败一次（§11.4 已记录同类竞态），单跑与紧随其后的全量重跑均通过。

新增测试（8 个，`core::provider::classifier::tests`）：

| 测试 | 锁住的行为 |
|---|---|
| `typesafe_system_one_maps_bool_questions_and_answers` | 端到端：URL `/systemone`、`Bearer`、请求体 `{model,state,questions}`（bool 问题在线为 `noul`、无 temperature）、三类答案解析、usage 按目录单价计费 |
| `malformed_answers_keep_usage_and_report_error` | 缺答案 → error 结果**仍带 usage**（请求已发出并计费） |
| `mismatched_answer_types_are_rejected` | 答案类型与问题类型不匹配（choice 问题回 score）→ error |
| `malformed_usage_is_ignored` | 字段非数字按 0；两字段都缺 / 非对象 / 无 usage → None |
| `missing_api_key_returns_error_result` | 缺凭据 → error 结果，不发请求 |
| `provider_error_returns_error_result` | 非 2xx（400）→ error 结果带状态码与响应体 |
| `non_classifier_model_and_unknown_api_are_rejected_before_request` | chat 模型 / 未实现 api 都在发请求前被拒（不联网） |
| `wire_question_and_url_shapes` | `base_url` 尾斜杠归一 + 三类问题的线上 `type`（choice/score/noul） |

改动文件：新增 `src/core/provider/classifier.rs`、`examples/classify.rs`；
修改 `core/provider.rs`（`pub mod classifier` + 类型再导出 + `classify` 入口 + 模块/`ModelType` 注释）、
`core/model_resolver.rs`（`list_models_of_type` 注释）、
`assets/docs/{models,sdk}.md`、`migrations/migration-v0.99.1.md`（本文档，含四.1 的纠正）。
未提交 commit（按项目规则：未显式要求不自动提交）。

---

## 十六、第十一轮：2.8 虚拟模型（宿主侧全链路）

按用户指定实现 2.8，参考 pi `packages/coding-agent/src/core/virtual-models.ts`
（导出 `VIRTUAL_MODEL_API` / `VIRTUAL_MODEL_STATE_ENTRY` / `ModelRouteReason` /
`ModelRouteRequest` / `ModelRoute` / `VirtualModelDefinition` / `withVirtualModels` /
`createVirtualModel` / `getBranchSelection` / `getVirtualModelState`）、
`ModelRuntime.registerVirtualModel/resolveModel` 与 `agent-session.ts` 的
`_installAgentRequestProjection`（每请求路由、状态条目、按路由模型压缩）。
未做 2.1/2.3 余项；2.9/2.10 已在第九/十轮落地，本轮不动。

### 16.1 新增 `core/virtual_models.rs`（类型 + 注册表 + 路由 + 状态）

- 常量：`VIRTUAL_MODEL_API = "prux-virtual"`、`VIRTUAL_MODEL_STATE_ENTRY =
  "prux.virtual-model-state"`、`STANDALONE_OWNER = ""`（SDK 直注册、不随扩展启停过滤）。
- `ModelRouteReason`（`user`/`continuation`/`retry`/`direct`）、`RouteTarget`（previous）、
  `FailedRoute`（retry）、`ModelRouteRequest`（selected / thinkingLevel / reason /
  previous / failed / state / messages / abort）、`ModelRoute`（路由器返回：provider +
  model_id + thinking_level + state）、`ResolvedRoute`（宿主校验后的物理 `ModelConfig`
  + 钳制级别 + state）。
- `VirtualModelRouter` trait（`route(&self, req) -> BoxFuture<Result<ModelRoute>>`）+
  闭包 blanket impl（`for<'a> Fn(ModelRouteRequest<'a>) -> BoxFuture<'a, _>`），
  扩展/SDK 用 `Arc<dyn VirtualModelRouter>` 挂进 `VirtualModelDefinition`；
  `with_thinking_levels/with_limits/with_input` 构造器 + 归一化
  （非法级别过滤、空则 `["off"]`；空 input 则 text+image）。
- 全局注册表 `(provider, id) → {definition, owner}`（BTreeMap，进程级单例）：
  `register/unregister/unregister_owner/clear/active_definitions/find_active/
  is_active_virtual/list_for_provider/virtual_only_providers`。归属扩展的条目随
  `extensions::is_extension_active` 过滤（禁用/`--no-extensions`/模式不匹配即从目录与
  路由中消失）；无归属恒可用。
- `previous_route(messages)`：跳过 error/aborted 与 api 为虚拟的响应（失败路由不产生
  previous）。`read_state`/`append_state`：`find_entries_on_branch(leaf, …, false)`
  从叶回溯取最近一条 `prux.virtual-model-state` custom 条目，写回经
  `Session::append_custom_entry`。
- `resolve_route` / `resolve_route_with`（可注入解析器，测试用）：查路由器 → 调用 →
  校验目标非虚拟、带凭据 → `clamp_thinking_level` 钳制；错误文案为
  `Virtual model <p>/<id> routed to <p>/<id>, <原因>` / `… is not registered.`。
  `resolve_physical` 是默认解析器（`model_resolver::configured_model` + 凭据非空校验）。

### 16.2 目录接入（`model_resolver.rs` / `auth.rs`）

- `find_model_impl`：虚拟模型**优先**匹配（id/name 精确 → 前缀回退），返回合成
  `ModelEntry`（`api = prux-virtual`、`thinking_level_map` 由级别列表逐值生成、cost 0），
  因此同名物理 chat 条目被隐藏（对齐 pi `withVirtualModels`）。
- `list_models_of_type(provider, Chat)`：先剔除与虚拟模型同名的物理条目，再追加
  `(id, name)`；image/classifier 列表不受影响。
- `default_model_for`：无物理条目时回退第一个虚拟模型。
- `auth::list_configured_providers`：追加**扩展注册**的、类别目录为空的 provider
  （虚拟-only provider「始终可用」）。SDK 直注册的虚拟-only provider 不在此列——
  否则任何进程内临时注册都会泄露进 UI 目录（本轮实测：早期版本会打挂
  `handlers::auth` 的 3 个面板用例），SDK 需先 `models.json` 声明该 provider。

### 16.3 扩展面（`extensions.rs` / `extensions/registry.rs`）

- `Extension::virtual_models() -> Vec<VirtualModelDefinition>`（默认空）；
  `register_extension_arc` 注册时收集（归属 = `ext.name()`），
  `unregister_extension` 经 `virtual_models::unregister_owner` 一并注销。
- 类型经 `core::extensions` 再导出（`VirtualModelDefinition` / `VirtualModelRouter` /
  `ModelRoute*` / `FailedRoute` / `RouteTarget`）。

### 16.4 Agent 侧每请求路由（`agent_session.rs` / `agent_loop.rs`）

- `Agent` 新增 `routed_model` / `routed_thinking_level` / `pending_failed_route`；
  新增 `selected_is_virtual` / `effective_model` / `effective_thinking_level` /
  `clear_routed_model` / `take_pending_failed_route` / `direct_model` /
  `summarization_model`。`reset_run_state` 清路由与失败记录。
- `run_turn`：注入 steering 后先 `clear_routed_model`，虚拟选择调用
  `resolve_virtual_route`（每请求一次）；成功则写 routed、必要时落 state 条目、广播
  `model_routed`，随后按路由后物理模型做一次阈值压缩（`start_turn`/`compact_before_run`
  对虚拟选择跳过压缩，对齐 pi `prepareRequest`）；失败则产出 `stopReason=error` 的
  assistant 消息并终止本轮。
- `route_user_turn` 判定 reason：`failed` → retry；最后一条 assistant 之后有 user →
  user；否则 continuation。`direct` 由 `direct_model`（压缩/扩展直调）走独立入口。
- `perform_llm_call`/`call_model`/cache warmer/`assistant_msg.thinking_level`/
  `prepare_model_context`（OAuth 刷新按有效 provider，写回 routed）/`emit_abort_failure`
  全部改用**有效模型/级别**；`run_default_loop` 对虚拟选择跳过 api key 前置检查
  （改为路由时校验物理凭据）。
- 压缩：`maybe_compact_inner` 的阈值判定、`CompactionSettings`、摘要 `max_tokens` 与
  `simple_completion` 都用 `summarization_model()`（进行中 = 路由后模型；空闲 = 按
  `direct` 路由）；`navigate_tree` 的 branch summary 在借用 session **之前**解析摘要模型
  （避免 `&mut session` 借用冲突）。`last_message_is_overflow` / `should_recover_overflow`
  用有效模型；`recover_from_overflow` / `retry_transient_errors` 在弹出失败消息前经
  `capture_failed_route` 记录 `FailedRoute`（虚拟 api 的消息跳过）。

### 16.5 TUI（footer 路由显示）

- `FooterCtx` 新增 `routed_model` / `routed_thinking_level`；`extensions/footer.rs`
  新增 `routed_suffix`（` → <物理模型>[:<级别>]`），normal/minimal（`stats_line`）与
  rich 的模型段统一追加。
- `App` 新增 `routed_model` / `routed_thinking_level` 镜像；sink 事件
  `model_routed` 更新、`agent_start` 与模型切换（`apply_model_mirror`）清空。

### 16.6 与 pi 的偏差（已知且刻意）

1. **示例插件已变成内置扩展**：pi 的 `examples/extensions/jev-router.ts` 在 prux 里是
   内置扩展 `jet`（第十二轮落地，见第十七节，默认不启用）；prux 没有 TS 扩展示例，
   路由逻辑写在 `Extension::virtual_models()` 里（文档给了完整 Rust 示例）。
2. **不单独暴露 `unregisterVirtualModel(provider, id)`**：随扩展整体注册/注销；
   `virtual_models::unregister` / `unregister_owner` 供 SDK 与测试用。
3. **注册同名物理模型不抛错**：pi 在物理模型已存在时 `registerVirtualModel` 抛错；
   prux 扩展在启动时注册、目录可能随后刷新，故改为「虚拟隐藏同名物理」（与 pi
   `withVirtualModels` 的隐藏语义一致），不拒注册。
4. **SDK 直注册的虚拟-only provider 不进已配置列表**（见 16.2）。
5. **`/resume` 不恢复模型选择**：prux 本就不按 `model_change` 恢复（pi 会），
   虚拟选择同样只随显式 `/model` 切换。
6. **`/proxy` 不路由虚拟模型**：网关按请求模型直接 `stream_chat`，虚拟条目落到
   `unsupported api type: prux-virtual`（provider 层不依赖 `virtual_models` 以维持分层）。
7. **无 `signal` / `onPayload` / `onResponse` / `maxRetries` 选项**：与 provider 层其它
   入口一致；取消经 `request.abort`（`Arc<AtomicBool>`）。
8. **`context_window`/`max_tokens` 未声明即 0（未知）**：与 pi 同；首次响应后按物理模型
   窗口统计上下文占用。

### 16.7 验证

```bash
cargo test                    # lib 2084 passed / 0 failed / 1 ignored；全部集成测试通过
cargo clippy --all-targets    # 仅剩既有 warning（settings_manager.rs:266、examples/bot/*）
cargo fmt --check             # 通过
```

> 既有 flake（与本轮无关，实测）：`modes::interactive::handlers::commands::tests::
> plan_todos_commands_follow_extension_lifecycle` 在全量并行跑时偶发失败（§13.4 已记录）；
> 本轮三次全量跑中失败 2 次、单跑均通过。

新增测试：

| 测试 | 锁住的行为 |
|---|---|
| `virtual_models::tests::definitions_normalize_levels_and_input` | 空级别→`["off"]`、空 input→text+image、非法级别被过滤 |
| `virtual_models::tests::registry_lists_and_unregisters` | 注册/列表/查找/重复注销返回 false |
| `virtual_models::tests::resolve_route_validates_target_and_clamps_level` | 目标解析注入、thinking 级别钳制到目标能力范围 |
| `virtual_models::tests::resolve_route_rejects_virtual_target_and_missing_registration` | 目标仍是虚拟模型 / 未注册的虚拟模型都报错 |
| `virtual_models::tests::previous_route_skips_failed_and_virtual_messages` | 跳过 error/aborted 与 api 为虚拟的响应 |
| `virtual_models::tests::state_roundtrips_through_session_branch` | 状态经 custom 条目落分支并按 `(provider,id)` 取回 |
| `model_resolver::tests::virtual_models_join_chat_catalog_and_hide_same_id_physical` | 虚拟进 chat 目录、同名物理被隐藏、`find_model` 返回 `prux-virtual`、默认模型回退虚拟条目 |
| `agent_session::tests::virtual_model_routes_request_and_records_physical_model` | 端到端：请求发给路由出的物理模型、assistant 记录物理模型、广播 `model_routed` |
| `agent_session::tests::virtual_model_routing_failure_ends_run_with_error_message` | 路由失败 → `stopReason=error` 的助手消息（消息指向虚拟模型），不发请求 |
| `footer::normal::tests::virtual_model_shows_routed_physical_model` | footer 渲染 `auto:high → gpt-real:medium`；未路由不显示箭头 |
| `handlers::events::tests::model_routed_event_updates_footer_mirror_and_agent_start_clears` | `model_routed` 更新镜像、`agent_start` 清空 |

改动文件：新增 `src/core/virtual_models.rs`、`assets/docs/virtual-models.md`；
修改 `core.rs`、`core/{model_resolver,auth,agent_session,agent_loop,extensions,extensions/registry,doc_sync}.rs`、
`extensions/{footer,footer/normal,footer/minimal,footer/rich}.rs`、
`modes/interactive/{app,render/status,handlers/events}.rs`、
`assets/docs/{index,extensions,models,sdk}.md`、本文档。未提交 commit
（按项目规则：未显式要求不自动提交）。

---

## 十七、第十二轮：`jet` 扩展（2.8 + 2.9 的开箱消费者）

按用户指定把 pi 的示例插件 `examples/extensions/jev-router.ts` 移植为内置扩展；
参考 pi `packages/coding-agent/examples/extensions/jev-router.ts` 与其
`test/jev-router-example.test.ts`。未做 2.1 / 2.3 余项（留待后续）。

### 17.1 新增 `src/extensions/jet.rs` + `src/extensions/jet/config.rs`

- **扩展形态**：内置扩展 `jet`（`PRIORITY_JET = 38`，排在 `tool_search` 之后），
  **默认不启用**（`default_enabled() == false`）、声明 `Dev` / `Creator` 两个模式
  （`Minimal` 下不可用）；`fork_project()` 声明移植自 `pi` 0.99.1。
  不提供任何模型可见工具，只通过虚拟模型与 `/jet` 命令暴露能力。
- **配置**（`agent_dir()/extensions/jet.json`，camelCase、带 `configVersion`，
  读写范式照 `notify.json`：缺失/缺键回写、损坏文件不覆盖、写盘 read-modify-write）：

  ```json
  {
    "classifier": { "provider": "openrouter", "model": "~typesafe/jev-latest" },
    "router": {
      "planning": { "provider": "openai", "model": "gpt-5.6-terra" },
      "planningComplex": { "provider": "anthropic", "model": "claude-opus-4-8" },
      "implementation": { "provider": "opencode", "model": "kimi-k3" }
    }
  }
  ```

  **每档各自带 `provider` + `model`，三档可分属不同 provider**（分类器同样是独立的一份）。
  **没有默认值**：空串 = 未配置；`classifier` 可整体缺（缺了分类器不参与）、
  `router.planningComplex` 可缺（缺了分类器一定不参与）。
- **注册门槛**（`build_definition`）：`router.planning` 与 `router.implementation` 两档
  配置完整（`provider`/`model` 都非空）、且每档都能在模型目录里解析到
  （`find_model_of_type(.., Chat)`、非虚拟条目）才注册 `jet/auto`；否则连 `jet` 这个
  provider 都不出现在目录里（`virtual_only_providers()` 自然为空）。凭据不查——路由时由
  宿主校验。
- **能力继承**：`contextWindow`/`maxTokens` 取 `planning` 与 `planningComplex` 中
  **较小**者（`0` = 未知/不限，不参与取小）；thinking 档位取三个目标模型能力的
  **交集**（跨 provider 时按各家实际能力取，空交集由定义归一化为 `["off"]`）。
  对齐 pi 的 `contextWindow: 272000` / `maxTokens: 128000` 硬编码，但改成从目录实测
  （pi 是示例插件，只能写死）。
- **路由决策**（`JetRouter::decide`，逐条对齐 pi）：
  1. `reason == direct` → `implementation`，不读不写状态；
  2. 状态 `phase == planning` 且本轮（最后一条 user 之后）有**成功**的
     `edit`/`write` 工具结果 → 切 `implementation` 并写状态 `{implementation, model}`；
  3. 有状态且 `state.model` 仍属当前配置 → 沿用，**返回 `state: None`**（不重复落条目，
     对齐 pi 的"不改状态就不传 state"）；
  4. 其余按新会话重判：`previous` 命中本配置的 `planning`/`planningComplex` 档
     （provider + 模型 id 都一致）→ 直接沿用（省一次分类调用与一次 prompt-cache miss）；否则调分类器
     （`choice` 题 `complexity`，`probabilities.complex >= 0.5` 判复杂），任何缺失/失败
     回退 `planning`；结果写状态 `{planning, model}`。
  - 状态 JSON 与 pi 同形：`{phase, model}`，由宿主落成 `prux.virtual-model-state` 条目；
    沿用判定按**模型 id** 认（相位 + 模型 id 都对得上即沿用），因此单独改某档的 provider
    不会打断已有会话，下一次请求就用新 provider。
  - `route_to` 的 provider 取自**目标档**：三档可分属不同 provider。
  - thinking 级别原样透传（宿主 `clamp_thinking_level` 兜底）；分类器用量**不入账**
    （pi 的 `ModelRuntime.classify()` 同样只把 usage 返回给调用方，jev-router 丢弃）。

### 17.2 `/jet` 命令（`busy_safe`，子命令补全 `set` / `router` / `reset`）

| 子命令 | 行为 |
|---|---|
| `/jet` | 打印注册状态（就绪 / 不可注册的原因）与分类器状态（未配置 / 可用 / 解析不到） |
| `/jet set <provider> <model-id>` | 覆盖 `classifier`；必须是 `type: classifier` 条目，否则拒绝且不落盘 |
| `/jet router [--provider <p>] [--planning <id>] [--planning-complex <id>] [--implementation <id>] [--planning-provider <p>] [--planning-complex-provider <p>] [--implementation-provider <p>]` | **增量**覆盖（只改给到的项，`--flag=value` 也接受）；`--provider` 一次给三档设同一 provider，`--*-provider` 单独覆盖某一档（同时给时单项优先）；未知 flag / 空值 / 未知 provider / 改动的档解析不到都拒绝且不落盘 |
| `/jet reset` | 清空两个小节；若当前会话正选中 `jet/auto`，按 `previous_route(messages)` 切回最近一次路由到的物理模型（拿不到则回落 `resolve_provider_model` 的兜底模型），并提示切到了哪儿 |

三条写命令写盘成功后**立即**调 `sync_registration()`（`register` 覆盖旧定义 / 不可注册
时 `unregister`），因此 `jet/auto` 在 `/model` 里即时出现或消失，不需要重启；被重新
启用时（`on_enabled_changed(true)`）也补一次同步。startup 路径仍走
`Extension::virtual_models()`（读配置 → 有效才返回定义）。

### 17.3 顺带修的宿主侧缺陷：仅有虚拟模型的 provider 默认模型解析

第十一轮的 `model_resolver::default_model_for` 在"provider 没有物理条目、只有虚拟模型"
时回退到 `virtual_models::list_for_provider(provider).next()`——而后者返回的是
`(id, 展示名)`，不是 `(provider, id)`，于是调用方拿到的 provider 变成了模型 **id**。

`jet` 是第一个内置的虚拟-only provider，因此这条路径第一次在真实使用中可达：
`prux --provider jet` / `PRUX_PROVIDER=jet`（不带 `--model`）会解析成
"provider `auto`"，报 `provider auto has no model catalog`，而不是列出 `jet/auto`。

已修为 `.map(|(id, _)| (provider.to_string(), id))`，并在既有用例
`model_resolver::tests::virtual_models_join_chat_catalog_and_hide_same_id_physical`
里补断言（此前只断言了 `list_models`，没断言 `default_model_for`，所以漏了）。

### 17.4 与 pi 的偏差（已知且刻意）

1. **每档自带 provider**：pi 示例把三档写死在同一家（`openai-codex` 的
   `gpt-5.6-sol`/`terra`/`luna`），而 prux 第四轮已删除 `openai-codex`；prux 做成
   `router.{planning,planningComplex,implementation}` **各带 `provider` + `model`**，
   三档可分属不同 provider（配置与 CLI 都支持，见 17.2）。分类器同理：pi 用独立 provider
   `typesafe/jev-latest`，prux **没有 `typesafe` provider**，只能指到装有 `type: classifier`
   条目的 provider（`openrouter/~typesafe/jev-latest`、`opencode/jev-1.13`、
   `vercel-ai-gateway/typesafe-ai/jev`）。
2. **`--planning-complex` 是可选 flag**：pi 的规划模型在复杂/普通两档之间由分类器二选一；
   prux 只给 `--planning` 时规划模型固定、分类器不参与（`/jet` 状态里如实显示），
   给了 `--planning-complex` 才是 pi 的完整语义。
3. **无默认配置**（用户拍板）：pi 示例的默认值是写死在代码里的三个模型；prux 不写默认，
   配置不完整就不注册——避免往 `/model` 里塞一个必然报错的条目。
4. **配置持久化**：pi 示例完全没有配置面（改代码），prux 提供 `jet.json` + 三条命令。
5. **`/jet reset` 的自动切走**：pi 没有配置就没有这个问题；prux 为了避免留下必然报错的
   选中项，reset 时切回最近一次路由到的物理模型。改配置（`set`/`router`）时则**保持选中**
   （虚拟条目仍在，只是规则变了，下一请求即生效）。
6. **无 `signal` 透传**：`provider::classify` 没有取消参数（见 15.4），
   路由过程中的分类调用不可取消（`request.abort` 只作用于目标请求）。
7. **分类器用量不入账**、**不接 `/extension` 设置面板**（模型 id 是自由文本，
   `ExtensionSetting` 只支持离散枚举）、**不加 `examples/` 示例**（扩展即入口）。

### 17.5 验证

```bash
cargo test                    # lib 2116 passed / 0 failed / 1 ignored；全部集成测试通过
cargo clippy --all-targets    # 仅剩既有 warning（settings_manager.rs:266、examples/bot/*）
cargo fmt --check             # 通过
```

新增测试（`extensions::jet::tests` 24 个 + `extensions::jet::config::tests` 8 个）：

| 测试 | 锁住的行为 |
|---|---|
| `state_round_trips_and_rejects_invalid` | `{phase, model}` 往返；缺字段 / 未知相位 / 空模型 → None（当作新会话重判） |
| `edited_this_turn_detects_only_successful_edits_after_last_user` | 只认最后一条 user 之后、`toolName ∈ {edit, write}`、`isError == false` 的工具结果 |
| `plan_target_prefers_complex_only_when_available` | 复杂判定只在配了 `planningComplex` 时改选；否则（含 Unknown）回落 `planning` |
| `tighter_ignores_zero` | 窗口/输出上限取小；`0` = 未知/不限，不参与取小 |
| `last_user_text_joins_blocks_and_truncates` | 取最后一条 user 的文本并截到 16000 字符；无 user → 空串 |
| `classifier_context_asks_the_complexity_choice` | 题面：`state.prompt` + 一道 `choice` 题 `complexity`（standard/complex） |
| `parse_router_args_accepts_both_forms_and_rejects_bad_input` | `--flag value` / `--flag=value`；缺值、未知 flag、无 flag 都报错 |
| `route_direct_goes_to_implementation_without_state` | `direct` → 实现模型、不碰状态、thinking 透传 |
| `route_new_session_plans_and_writes_state` | 无状态无分类器 → `planning` + 落状态条目 |
| `route_reuses_previous_planning_model` | `previous` 命中本配置的规划档（provider + 模型都一致，含复杂档）即沿用；provider 不同不沿用 |
| `targets_may_come_from_different_providers` | 三档分属不同 provider 时各档各自解析/各自路由（实现档走自己的 provider）；thinking 交集按各家能力取 |
| `state_follows_target_after_provider_change` | 只改某档 provider（模型 id 不变）时，已有状态继续沿用该档并使用新 provider |
| `route_switches_to_implementation_after_first_edit` | 首次成功编辑 → 实现模型 + 新状态；已在实现阶段则不再动 |
| `route_keeps_state_model_without_rewriting_state` | 沿用状态时返回 `state: None`（不重复落条目） |
| `route_replans_when_state_model_left_the_config` | 状态里的模型已不在配置中 → 按新会话重判 |
| `definition_gate_requires_complete_and_resolvable_targets` | 缺必填档 / 半配（只有 provider 没 model）/ 未知模型 / 未知 provider / 图片条目 / 分类器条目 / 虚拟模型自己 都拒绝 |
| `definition_inherits_limits_and_thinking_levels` | 窗口与输出上限取规划两档较小者；thinking 档位取三个目标交集 |
| `virtual_models_follow_configuration` | 配置无效 → 不返回定义；配全 → 恰好一个 `jet/auto`；清空 → 又不返回 |
| `router_command_writes_incrementally_and_rejects_bad_input` | 增量写盘（`--provider` 批量设三档、`--*-provider` 单项覆盖）；未知 flag / 缺失模型 / 未知 provider（含 `--*-provider`）都拒绝且不落盘；补齐后 `ready`；批量与单项同时给时单项优先 |
| `set_command_requires_a_classifier_entry` | 参数个数、非 `type: classifier` 条目拒绝；分类器条目接受 |
| `reset_clears_config_and_switches_away_from_virtual` | reset 清空两个小节并注销；正选中 `jet/auto` 时切回物理模型并提示，未选中时不切换 |
| `switch_target_prefers_previous_physical_model` | 切回目标优先取 `previous_route`，无物理响应时回落兜底模型 |
| `classifier_picks_complex_planning_model` | 端到端（假分类器服务）：`POST /systemone`、题面是最后一条 user 文本、复杂判定 → `planningComplex` |
| `classifier_failure_falls_back_to_plain_planning` | 分类器 HTTP 失败 → 回退 `planning`，不报错 |
| `config::tests::{merge_*, set_and_load_round_trip, targets_lists_configured_ones_in_order, reset_*, load_backfills_missing_keys, corrupt_file_is_not_overwritten}` | 配置合并（含缺 `model` 视为未配置）/回写/损坏保护；`targets()` 只列已配置档、`targets_all()` 恒三档 |

集成测试同步更新：`tests/extensions_registry.rs` 内置扩展计数 17 → 18、
清理名单与注册顺序补 `jet`、并断言 `jet` 应默认不启用。

端到端（真实二进制 + 临时 `PRUX_AGENT_DIR`，走 `/model` 面板同一份 `list_models`）：

| 场景 | 命令 | 结果 |
|---|---|---|
| 扩展启用 + `jet.json` 完整（三档可分属不同 provider） | `PRUX_PROVIDER=jet prux --list-models` | `auto: Auto (Jet)` |
| 同上，且带 `--model` | `prux --provider jet --list-models` | `auto: Auto (Jet)` |
| `jet.json` 缺失 | 同上 | `Error: unsupported provider: jet` |
| 扩展被禁用（`enabledExtensions: []`） | 同上 | `Error: unsupported provider: jet` |
| `jet.json` 的 `--implementation` 指向不存在的模型 | 同上 | `Error: unsupported provider: jet` |

> 未做「`/jet router` 成功后立刻出现在 `/model` 列表」的直接断言：那需要在本进程内
> 注册并**启用**一个归属扩展的虚拟-only provider，会经 `virtual_only_providers()` →
> `list_configured_providers()` 泄露进并行运行的 `/model` 面板用例（文档 16.2 记录过同类
> 事故）。该行为改由注册门槛（`build_definition` / `Extension::virtual_models()`）与宿主侧
> 既有虚拟模型用例覆盖。

改动文件：新增 `src/extensions/jet.rs`、`src/extensions/jet/config.rs`；
修改 `extensions.rs`（`pub mod jet` + `PRIORITY_JET`）、
`core/model_resolver.rs`（17.3 的 `default_model_for` 修复 + 回归断言）、
`tests/extensions_registry.rs`、`assets/docs/{extensions,virtual-models}.md`、本文档。
未提交 commit（按项目规则：未显式要求不自动提交）。

---

## 十八、第十三轮：2.1 codemode 的实现形式（文档轮，未改代码）

按用户指定只做形式分析：读 pi 的 `packages/codemode/`（沙箱库）与
`packages/coding-agent/src/extensions/codemode/`（内置扩展），不改任何代码。

**结论**：codemode 在 pi 里是 **内置扩展**（`builtin:codemode`）注册的**一个工具**（名字也叫
`codemode`），扩展名与工具名 **1:1**；**不是** `core/tools/` 里 read/bash/edit/write 那批内置工具。

### 18.1 pi 侧的三层结构

| 层 | 位置 | 职责 |
|---|---|---|
| 沙箱库（不认识 pi 的工具） | `pi/packages/codemode/`（`@earendil-works/pi-codemode`，`src` 684 行） | QuickJS-wasi VM 的执行与通信。`runtime/worker.ts` 在 node `worker_threads` 里建 VM（`memoryLimit` 256 MB、`maxStackSize`、`Atomics` 共享的中断标志、丢弃 fd 1/2 写）；`runtime/host.ts` 是主线程侧句柄（默认 300 s 超时、`store` 序列化、`RESERVED_GLOBALS` 校验、worker 生命周期）；`runtime/prelude-source.ts` 是注入沙箱的 JS 预置源码；`runtime/protocol.ts` 是 host↔worker 消息；`declarations.ts` 生成工具声明文本与 MCP 共享类型；`source.ts` 解析 `// @options:` 首行并提供约束采样用的 lark 语法；`wasm.ts` 编译 `quickjs.wasm` |
| **内置扩展**（唯一接进 agent 的地方） | `pi/packages/coding-agent/src/extensions/codemode/`（1079 行） | `index.ts` 的 `createCodemodeExtension()` 只做一件事：`pi.registerTool({ ...createCodemodeToolDefinition(...), defaultActive: false })`。`tool.ts` / `execute.ts` / `renderer.ts` / `worker.ts` 都是这**一个工具**的组成 |
| 加载登记 | `src/extensions/index.ts` 的 `builtInExtensions` | `{ name: "codemode", factory, replaceable: true, builtin: true }` → 2.7 的 `builtin:codemode` 命名、`-builtin:codemode`、`--no-extensions`、`-e builtin:codemode` 都作用在这一层（`replaceable`：第三方扩展注册同名 `codemode` 则接管） |

即：**扩展是加载/禁用单位，工具是模型可见面**；因为该扩展注册的工具数恰好是 1，
两种粒度在 pi 里等价（`--tools codemode` / `defaultTools: ["+codemode"]` 实际作用在**工具**上，
`pi.registerTool` 的 `defaultActive` 是同层机制，见 18.2）。

### 18.2 工具属性（`extensions/codemode/tool.ts`）

| 属性 | 值 | 作用 |
|---|---|---|
| 名 / label | `codemode` | 扩展名与工具名同名 |
| 参数 | `{ code: string }` | **裸 JS 源码**（不是 JSON 转义字符串、不是 markdown 围栏）；首行可写 `// @options: {"max_output_tokens":1000,"timeout_ms":60000}` |
| `exposure` | `"model-only"` | 模型可见，但**禁止嵌套调用**（注释原文 "Scripts must not start other scripts"，即脚本里不能再调 `codemode`） |
| `defaultActive` | `false` | 注册即**未激活**。激活途径：`--tools codemode`（整体替换选择）、`defaultTools: ["+codemode"]`、`setActiveTools()`；此外 MCP 扩展在「MCP 工具只能从脚本触达」时**自动激活**它（`autoEnableCodemode`） |
| `prepareLoadout(loadout)` | 有 | 每次激活集合变化时**重算自己的 description**：`codemode.mode`（`on`/`only`）决定 direct 工具的声明是「内联进各自 description」还是「从模型侧隐藏、改列进 codemode description」；`codemode.inlineBudget`（默认 3000 估算 token，字符 ÷ 4）限制工具声明的内联预算，放不下的只列 namespace 计数并提示用 `searchTools()` —— **这正是 prux 2.3 余项里缺的 `prepareLoadout()`** |
| `constrainedSampling` | `{ type: "grammar", variants: { openai_lark: CODEMODE_SOURCE_GRAMMAR } }` | 让支持的模型直接吐裸 JS；prux 只有 `ExtensionTool::constrained_sampling: bool`（JSON schema strict），无 grammar 变体 |
| 加载时机 | `execute.lazy.ts` → `import("./execute.ts")` | 沙箱与 wasm **首次调用**才载入，不在启动时 |

### 18.3 沙箱内模型/脚本可见的世界

- 全局：`tools.<name>(args)`（工具名归一为合法 JS 标识符，如 `mcp__ologs__get_profile`）、
  `ALL_TOOLS`、`searchTools(query, { limit, namespace })`（BM25，默认 8）、`describeTool(name)`、
  `text()` / `image()` / `exit()` / `store()` / `load()`、`console.*`、顶层 `return`；
  可选 `models.{ getModelsOfType, getAvailableOfType, getModelOfType, classify }`。
- **脚本可调集合**：agent loop 里**激活的 `direct` 工具** + **全部** `codemode` / `deferred` 工具
  （与是否声明给模型无关）；`model-only`（含 `codemode` 自己）与 `hidden` 不可
  （`agent-session.ts:1488` 的 `_getCallableTools`）。
  嵌套调用统一走 `ctx.executeTool`，因此校验 / `tool_call`·`tool_result` hook / 权限与直接调用一致；
  只有脚本输出给模型，嵌套结果不给；`nestedCalls` 记进工具 details（id 形如 `<codemode call id>/<n>`）。
- 结果解析：声明了 `outputSchema` 的工具 → 返回 `structuredContent`（带 `isError` 的错误结果若带也给，
  MCP 工具即走这条）；其余返回文本；失败/被拒/参数非法 → reject（携带工具错误文本）。
- `store()/load()` 跨调用持久：**成功**的脚本把写入 append 成 `codemode-store` 自定义条目，
  按分支可见（每个分支只看到自己路径上的写入）。
- `models.classify` 每脚本并发上限 4，usage 经 `combineUsage` 并入 codemode 结果 → 计入会话成本。
- bash 面向**程序化调用方**（codemode 脚本）的结构化输出上限 `STRUCTURED_OUTPUT_MAX_BYTES = 1 MiB`
  （超出保留首尾各 512 KiB），模型侧仍是原来的截断预算。

### 18.4 映射到 prux（若实施）

| pi | prux 对应结构 | 差距 / 动作 |
|---|---|---|
| `createCodemodeExtension` | **新增 `src/extensions/codemode.rs`**（实现 `Extension`，`tools()` 返回单个 `ExtensionTool`） | 无缺口。**不要**塞进 `core/tools/`（那是内置工具，形态不同） |
| `defaultActive: false` | `Extension::default_enabled() == false`（prux 已有：`debug_provider` / `plan_mode` / `tool_search` / `proxy` / `jet`… 都在用） | 粒度等价（该扩展只有一个工具）→ **不要**新造 per-tool 的 `default_active` |
| `exposure: "model-only"` | `ToolExposure::ModelOnly`（已存在，语义逐字一致：模型可见、拒绝嵌套调用） | 无缺口 |
| 脚本可调集合（`_getCallableTools`：`codemode` \| `deferred` \| 已激活的 `direct`） | prux 现只有一把尺子：`ToolExposure::is_nested_callable`（`Direct`✅、`Codemode`✅、`Deferred`激活后✅、`ModelOnly`❌、**`Hidden`✅**） | pi 里 `hidden` **不可**被脚本调；prux 的 `Hidden` 定义本就是「宿主导内部工具，可经 `execute_tool` 调」→ 实施 codemode 时需把「脚本可调集合」与「扩展可嵌套调集合」**拆成两套**（否则 `Hidden` 会混进 `tools` 表） |
| `defaultTools: ["+codemode"]` 激活 | prux 的 `defaultTools` **只选内置工具**：`ToolSelection::Default(names)` 对扩展工具是「全并入」（`agent_session.rs:104-128`）；扩展粒度的开关只有 `/extension` 面板与 `disabledExtensions` | **不改（用户拍板，第十九轮确认）**：prux 的 `defaultTools` 只处理内置工具，`codemode` 属扩展工具，不进 `defaultTools`；pi 的 `"+codemode"` 在 prux 里等价于「启用 codemode 扩展」（`enabledExtensions`/`/extension`），不是缺一个工具名。原写的「2.7 口径遗留，需让扩展工具也进 defaultTools」已作废 |
| `prepareLoadout()` | **无**。prux 在 `rebuild_tools()` 时经 `compose_tools`（`agent_session.rs:164`）整体重算工具表与系统提示 | 需给 `Extension` 加「按 loadout 改写工具 description」的钩子（等价落点：`visible_extension_tools` 之后对 `codemode` 重算 description）。**2.1 的硬前置** |
| `constrainedSampling: grammar` | 只有 `ExtensionTool::constrained_sampling: bool`（`core/tools/index.rs`） | 无 grammar 变体；功能上不必需（影响模型是否愿写裸 JS） |
| node `worker_threads` + `quickjs-wasi` | 进程内 rquickjs：`AsyncRuntime` + `set_interrupt_handler`（`subagent/workflow/bridge.rs:862`） | **无独立线程可硬杀**，且**未设内存上限**（pi 设 256 MB）→ 移植时需补内存上限并沿用 workflow 已有的墙钟兜底；JS 侧 glue 必须重写（pi prelude 暴露 `tools.<name>()`，prux workflow glue 是 `agent()`/`__dispatch`/`__settle`） |
| MCP 工具的默认 exposure | prux `extensions/mcp.rs:324` 注册为 `ToolExposure::Direct`；pi 默认 `"exposure": "codemode"` + `autoEnableCodemode` | 独立拍板项（改默认会改 MCP 工具的模型可见面，且牵连 codemode 自动激活） |
| bash 1 MiB 结构化输出 | `core/tools/bash.rs` 已返回 `truncated` / `full_output_path`，但预算硬编码（`utils/truncate.rs`：50 KiB / 2000 行） | 需参数化截断预算 + 新增「程序化调用方」通道（模型侧不变） |

### 18.5 结论

- **形态**：pi 的 codemode = 内置扩展（`builtin:codemode`，`replaceable`）+ 一个
  `exposure = model-only` 的工具（默认未激活）。prux 的正确落点是
  **`src/extensions/codemode.rs`（`default_enabled() == false`）+ 单个 `ToolExposure::ModelOnly`
  的 `codemode` 工具**，与 `core/tools/` 的内置工具无关。
- **工作量分布**：不在「注册成什么」，而在 JS 侧 glue 重写（复用 `subagent/workflow` 的 rquickjs 桥）、
  `prepareLoadout()` 等价物（2.3 余项，且为硬前置）、「脚本可调集合」与「扩展可嵌套调集合」拆分、
  bash 截断预算参数化、`exposure = codemode` 的沙箱可见面；MCP 默认 exposure 与 grammar
  约束采样为可选/独立拍板项。
- 本轮为纯文档轮，未改代码，故无 `cargo` 验证；结论的 pi 侧依据是
  `pi/packages/coding-agent/src/extensions/{index.ts,codemode/*}`、
  `pi/packages/codemode/src/**`、`pi/packages/coding-agent/docs/{cli,settings}.md`
  与 `src/extensions/mcp/index.ts`。

---

## 十九、第十四轮：2.1 codemode 落地（含 2.3 的 `prepareLoadout()`）

按第十三轮定案的形态（第十八节）实现 codemode：**一个内置扩展注册一个工具**
（扩展名 = 工具名 = `codemode`），并先补上它的硬前置 `Extension::prepare_loadout()`。
参考 pi `packages/codemode/src/**` 与 `packages/coding-agent/src/extensions/codemode/**`。

### 19.1 新增文件

| 文件 | 作用 |
|---|---|
| `src/extensions/codemode.rs` | 扩展本体：工具定义（`{code: string}`、`exposure = ModelOnly`）、`prepare_loadout`、`execute_tool_async`（脚本执行 + 结果渲染）、`on_session_start`（`store` 快照重建）、设置读取（`codemode.mode` / `inlineBudget`） |
| `src/extensions/codemode/sandbox.rs` | rquickjs 沙箱驱动（对齐 pi `CodemodeSandbox` + worker）：`__call`/`__output`/`__done` 三个宿主函数 + `__settle` 回灌、中断处理器（取消/墙钟/脚本 `timeout_ms`）、256 MB 内存上限、**空转检测**（脚本等一个永不能 settle 的 Promise 时判死，文案与 pi `stalled()` 一致） |
| `src/extensions/codemode/glue.js` | JS 预置（由 pi `prelude-source.ts` 改写）：`tools` / `ALL_TOOLS` / `text` / `image` / `exit` / `store` / `load` / `console` / 命名空间全局，桥换成 rquickjs 宿主函数 |
| `src/extensions/codemode/source.rs` | `// @options:` 首行解析（`max_output_tokens` / `timeout_ms`），逐值对齐 pi `source.ts`（含英文错误文案与行号保持） |
| `src/extensions/codemode/declarations.rs` | JSON Schema → TypeScript 声明（`toCodemodeIdentifier` / `schemaToType` / `renderToolSignature` / `renderToolSample`），对齐 pi `declarations.ts`；**不做** MCP `CallToolResult` 特判 |
| `src/extensions/codemode/description.rs` | 工具描述生成（pi `createCodemodeDescription` + `prepareCodemodeLoadout`）：指令段、按命名空间分组、`inlineBudget` 预算选择（每轮每组放最便宜的一个）、`on`/`only` 两种模式的描述改写与 `hidden_declarations` |
| `src/extensions/codemode/store.rs` | `store()`/`load()` 的跨调用持久：会话自定义条目 `codemode-store`（`{set, delete}`）+ 进程内快照 |
| `assets/docs/extensions/codemode.md` | 用户文档（脚本能力、首行选项、嵌套记账、模式/预算、与 pi 的差异） |

### 19.2 核心改动（2.3 余项的 `prepareLoadout` 等价物）

- `core/extensions/tools.rs`：
  - 新增 `ToolLoadout { declared, callable }` 与 `ToolLoadoutChanges { descriptions, hidden_declarations }`；
  - 新增 `ToolExposure::is_script_callable(activated)`：`direct`/`codemode` 可调、`deferred` 需激活、
    `model-only`/`hidden` 不进脚本工具表（与「扩展嵌套可调」的 `is_nested_callable` 分开——
    后者对 `hidden` 是允许的，那是「宿主/扩展内部工具」的既有语义）；
  - `ToolExecCtx` 新增 `script_tools: Arc<Vec<ExtensionTool>>`（pi `ExtensionToolContext.tools`）。
- `core/extensions.rs`：`Extension::prepare_loadout(&ToolLoadout) -> ToolLoadoutChanges`（默认空）。
- `core/agent_session.rs`：
  - `compose_tools` 返回 `(system_prompt, tools, hidden_declarations)`，尾部新增
    `apply_prepare_loadout`：按当前工具表算出 `ToolLoadout`（内置 + 可见扩展工具为 `declared`，
    脚本可调集合为 `callable`），执行各扩展的 `prepare_loadout`，把描述覆盖写回工具表；
  - `Agent.hidden_declarations` 字段；`build_exec_ctx` 填 `script_tools`（`script_tools_for`：
    激活的内置工具 + 按 `is_script_callable` 判定的扩展工具 + 注入工具；排除 `codemode` 自己）。
- `core/agent_loop.rs`：`perform_llm_call` 按 `hidden_declarations` 过滤本次请求的工具表
  （缓存保温请求同样用过滤后的表）；工具仍激活、仍可被脚本/嵌套调。
- `core/settings_manager.rs`：新增 `settings_section(key)`（全局 + 项目逐键合并）。
- `extensions/tool_search.rs`：`tool_document` 改 `pub`（`searchTools` 复用同一套 BM25 与分词）。

### 19.3 与 pi 的偏差（已知且刻意）

> 本节是第十四轮的快照；其中 2/3/5/7 已在第十五轮消解（见 20.7），6 也已随 20.3 的
> `ToolResult.is_error` 一并消解（第十九轮复核订正），4 的理由已于第十九轮改写。

1. **无独立 worker 线程**：pi 用 `worker_threads` + QuickJS-wasm，可 `terminate()` 硬杀；prux 是进程内
   rquickjs，改由中断处理器（取消标志 / 脚本 `timeout_ms` / **30 分钟兜底墙钟**）+ 256 MB 内存上限
   约束（pi 默认 `timeoutMs: Infinity`，其安全性来自可终止的 worker）。
2. **无 `models.*` 全局**：pi 的 `options.models` 默认 `true`（脚本可列模型目录、跑分类器）；
   prux 未接（等价 `options.models = false`）。2.9 的 `provider::classify` 已就位，接上只是加全局。
3. **无 `constrainedSampling: grammar`**：prux 的 `constrained_sampling` 只有 JSON schema strict，
   没有 pi 的 `openai_lark` 源码语法；只影响「模型是否愿意直接吐裸 JS」。
4. ~~**`store()` 快照按整个会话**（pi 按当前分支）~~ → **第二十轮已消解**（25.3）：
   快照改为每次脚本开跑前按当前分支重建（`ToolExecCtx::session_branch_entries` ←
   `Session::get_leaf_id()` + `Session::get_branch()`），本进程内未落盘的写入仍留在快照里
   （headless/SDK 无 UI 回执时不会被丢掉）。原文对“技术障碍”的描述曾写错：分支 API 本就在
   （`core/session_v4.rs`，`virtual_models::read_state` 已在用），缺的是 `ToolExecCtx` 的 session 句柄。
5. **无实时嵌套进度 / TUI 卡片**：pi 每次嵌套调用 `publish()` 一次进度并有 codemode 卡片；
   prux 没有 `onUpdate` 通道，嵌套记录在结束后统一进 `details.nestedCalls`（TUI 目前也不渲染它）。
6. ~~**失败结果不带图片附件**：prux 的错误结果只带文本（`ToolResult`/`ToolError` 二分支），
   pi 的失败结果仍保留已产生的 image 条目。~~
   → **已消解**（第十五轮 20.3 引入 `ToolResult.is_error` 后，失败同样走带 `is_error` 的
   `ToolResult`：`extensions/codemode.rs` 把脚本已产生的文本、`attachments` 与分类器用量
   一并回传；第十九轮复核确认该条已过期）。
7. **bash 的 1 MiB 结构化输出通道未做**：脚本拿到的 bash 结果仍是 50 KiB / 2000 行截断的文本。
8. **激活方式是扩展粒度**：prux 的 `defaultTools` 只选内置工具，故用
   `default_enabled() == false` + `/extension` 面板（或 `enabledExtensions`）开关，
   而不是 pi 的 `defaultTools: ["+codemode"]`。**这是用户拍板的刻意口径**（第十九轮确认）：
   `codemode` 属扩展工具，不进 `defaultTools`；pi 的写法在 prux 里等价于「启用该扩展」。

### 19.4 验证

```bash
cargo test                    # lib 2160 passed / 0 failed / 1 ignored；全部集成测试通过
cargo clippy --all-targets    # 仅剩既有 warning（settings_manager.rs:266、tasks/widget.rs、examples/bot/*）
cargo fmt --check             # 通过
```

新增测试（43 个，分布在 `extensions::codemode::*` 与 `core::agent_session::tests`）：

| 测试 | 锁住的行为 |
|---|---|
| `sandbox::calls_tools_and_returns_value` / `tool_error_rejects_in_script` | `tools.<name>()` 取值、工具错误 → 脚本内 Error、失败仍保留已产生的输出 |
| `sandbox::collects_output_items` / `exit_finishes_successfully` | `text`/`console`/`image` 条目与顺序、`exit()` 立刻成功结束 |
| `sandbox::store_and_load_round_trip` | `load()` 读快照、`store()` 回报写入（含 `undefined` 删除） |
| `sandbox::globals_receive_all_arguments` | 全局以 spread 收全部实参（`searchTools(query, opts)`） |
| `sandbox::script_throw_reports_error` / `syntax_error_is_a_script_failure` | 抛错/语法错误都是 `Script` 类型（不是沙箱失败），带 name/message/stack |
| `sandbox::stalled_promise_is_reported` | 等永不能 settle 的 Promise → 卡死判定（文案与 pi 一致） |
| `sandbox::timeout_is_reported` / `cancel_flag_aborts` | `timeout_ms` → `Timeout`；取消标志 → `Aborted` |
| `sandbox::concurrent_calls_run` / `tools_table_is_frozen` | `Promise.all` 并发、`tools` 表冻结 |
| `source::*`（4 个） | 首行选项解析、行号保持、全部非法输入的错误文案 |
| `declarations::*`（6 个） | 标识符归一、类型渲染（联合/组合器/`$ref`/递归/超长退化）、签名与样例 |
| `description::*`（9 个） | 预算选择、命名空间分组、deferred 不内联、`on`/`only` 两种装载改写 |
| `codemode::tool_definition_shape` / `truncation_*` / `error_text_includes_stack_and_call_summary` | 工具定义、输出截断与落盘、失败文案（栈 + 调用摘要） |
| `store::writes_update_snapshot_and_delete` / `empty_writes_are_noops` | 写入/删除语义与空写入无副作用 |
| `agent_session::codemode_prepare_loadout_inlines_callable_tools` | 装配工具表：`codemode` 描述内联非 direct 可调工具、direct 工具描述附声明、关闭扩展后复原 |
| `agent_session::codemode_only_mode_hides_direct_declarations` | `codemode.mode = only`（临时 settings.json）：可调工具的声明进 `hidden_declarations`，且全部列进 `codemode` 描述 |
| `agent_session::codemode_script_orchestrates_nested_tools` | 端到端：模型调 `codemode` → 脚本经 `tools.read()` 调内置工具 → 输出回模型、`details.nestedCalls` 记录、嵌套结果不单独成为会话消息 |
| `tests/extensions_registry.rs` | 切片收集数 19（新增 codemode）+ 禁用名单 |

改动文件：新增 `src/extensions/codemode.rs`、`src/extensions/codemode/{sandbox,glue.js,source,declarations,description,store}.rs`、
`assets/docs/extensions/codemode.md`；修改 `src/extensions.rs`（`pub mod codemode` + `PRIORITY_CODEMODE = 39`）、
`src/extensions/tool_search.rs`、`src/core/extensions.rs`、`src/core/extensions/tools.rs`、
`src/core/agent_session.rs`、`src/core/agent_loop.rs`、`src/core/settings_manager.rs`、
`assets/docs/{extensions.md,settings.md}`、`tests/extensions_registry.rs`、本文档。
未提交 commit（按项目规则：未显式要求不自动提交）。

## 二十、第十五轮：2.1 余项全清 + 2.2 的 MCP exposure 配置面

本轮把第十九节 19.3 里挂着的 2.1 余项（bash 结构化输出、`models.*`、`constrainedSampling: grammar`、
TUI 嵌套进度）与 2.2 的 exposure 配置面一次做完，并顺带修掉两个既有 bug。

### 20.1 bash / powershell 的 1 MiB 结构化输出（2.1 余项）

对齐 pi 的 `STRUCTURED_OUTPUT_MAX_BYTES = 1 MiB`：

- `core/tools/bash.rs`：新增 `BashStructuredOutput`（`output` / `truncated` / `full_output_path` /
  `exit_code` / `wall_time_seconds`，serde snake_case）与 `BashOutputCollector::read_full_output(max)`；
  超限时首尾各半截断，中间插 `[... N bytes omitted ...]`（切在字符边界上，不会产出非法 UTF-8）。
- `BashOutcome.structured: Option<Value>`：只有拿到 `exit_code` 时才生成（超时/取消仍是 `Err`），
  wall time 用 `Instant` 计时取 0.1s 精度。
- `core/tools/index.rs`：`ToolDef.output_schema` + `shell_output_schema()`，bash/powershell 声明它；
  `agent_session::builtin_tool_descriptor` 透传；内置 bash/powershell 的非零退出改为
  `Ok(ToolResult::error(text))` 且带上 `structured_content`（此前直接 `Err`，结构化结果会丢）。
- `powershell.rs` 同款。

**顺带修掉的两个既有 bug**（都在本项路径上，不修则功能不成立）：

1. **大输出死锁**：原实现串行读 stdout → stderr，stderr 未关闭且无数据时，stdout 写满 64 KiB
   管道即两边互等（`yes x | head -n 700000` 必现，且会连带卡住整个 agent）。改为**每流一个
   tokio task** + `mpsc` 汇总到单一消费 task，保持 `on_chunk` 串行与输出顺序。
2. **`full_output_path` 从未出现**：bash 取结果时 `clone()` 了 collector，而 tempfile 不参与
   `Clone` → 截断提示里的「Full output: 路径」永远是空的。改为 `Arc::try_unwrap` 取回原位。

### 20.2 `models.*` 全局（2.1 余项）

对齐 pi 的 `createModelGlobals`：

- `getModelsOfType(type)` / `getAvailableOfType(type)` / `getModelOfType(type, provider, id)` /
  `await classify(model, {state, questions})`，全部以 spread 收实参（与既有全局一致）。
- 类型/参数校验文案对齐 pi（`Unknown model type "x". Use "chat", "image", or "classifier".`、
  `provider must be a string`）。
- 分类器并发闸门 `MAX_CONCURRENT_MODEL_CALLS = 4`；失败的分类结果**不抛错**而是回传
  `stopReason = "error"` + `errorMessage`（与 pi 一致，脚本可自行判断）。
- usage 经 `combine_usage` 并入 codemode 结果与会话成本；每次 `models.*` 调用记进
  `details.modelCalls`（`status` / `args` / `durationMs` / `usage`），失败信息截断到 500 字符。
- `core/provider/usage.rs` 新增 `combine_usage(a, b)`（原 `agent_session::add_usage` 私有实现提上来复用）。

### 20.3 `isError` 独立字段（2.1 余项的隐藏前置）

pi 的 `toScriptValue`：有 `outputSchema` + `structuredContent` 时（**含错误结果**）脚本拿到结构化值，
否则 `isError` 才 throw。prux 此前用「文本是否以 `Error:` 开头」猜，会把带结构化输出的失败结果判成成功：

- `ToolResult.is_error: bool` + `ToolResult::error(text)` / `with_error()`；
- `FinalizedToolCall::from_tool_result`、`run_nested_tool` 都改读该字段；
- `codemode::Host::script_value` 改为 `Result<Value, String>`：结构化优先，其次 `is_error` → `Err(text)`。

### 20.4 `constrainedSampling: grammar`（2.1 余项）

pi 的 grammar 工具：模型声明 `compat.supportsOpenAIGrammarTools` 时，把工具的**单个必填 string 参数**
当裸文本下发（`type: "custom"` + lark 语法），省掉 JSON 转义。

- 类型与声明：`extensions::GrammarSampling { openai_lark, openai_regex }` + `ExtensionTool.grammar_sampling`；
  `ModelEntry.supports_openai_grammar_tools`（读目录 `compat.supportsOpenAIGrammarTools`）+
  `model_resolver::supports_openai_grammar_tools(provider, id)`；
  `codemode/source.rs` 新增 `CODEMODE_SOURCE_GRAMMAR`（对齐 pi 的 lark 定义）。
- **传递方式（与 pi 的偏差）**：prux 的工具表是 `(名字, 描述, schema)` 三元组，没有工具级元数据槽位，
  因此 `compose_tools` 把语法约束与承载属性名写进 schema 的 `x-prux-grammar` 键（grammar 工具本就
  不把 schema 发给模型，不会外泄）。provider 层读它决定是否转 `custom` 工具。
- **completions 路径**：`convert_tools_with_capabilities` 输出 pi 的
  `{type:"custom", custom:{name, description, format:{type:"grammar", grammar:{syntax, definition}}}}`；
  流式把**累积**的 `custom.input` 包装成 `{"<prop>":"…"}` 的 JSON 参数（首块补前缀、流末补 `"}`）；
  历史回放把 grammar 工具的 tool call 回放成 `{type:"custom", custom:{name, input}}`。
- **responses 路径**：工具为 `{type:"custom", name, description, format:{type:"grammar", syntax, definition}}`；
  处理 `response.custom_tool_call_input.delta/done`（delta 是增量、done 给完整值）与
  `output_item.done` 的 `custom_tool_call`；历史回放为 `custom_tool_call` / `custom_tool_call_output`，
  item id 前缀按 `ctc_` 保留（function 仍是 `fc_`）。

### 20.5 MCP exposure 配置面（2.2，B 方案：默认不变）

用户拍板保留 prux 默认的 `direct`，只补配置能力面：

- `ServerEntry.exposure`（`direct`（默认）/ `codemode` / `codemode-deferred` / `deferred` / `hidden`）、
  `ServerEntry.toolExposure`（逐工具覆盖，支持 `*` 通配；精确名优先，其次首个匹配模式）、
  顶层 `autoEnableCodemode`（默认 true）；非法 exposure 取值在解析时报错。
- `ServerEntry::exposure_for(tool)` / `uses_codemode_exposure()`（对齐 pi 的 `getMcpToolExposure`）。
- **`mcp` 工具自身的 exposure 按服务器聚合**（prux 把 MCP 工具聚合成单个 `mcp` 工具）：
  取最可见的一个（`direct` > `deferred` > `codemode` > `hidden`）。
- **逐工具 exposure 在 `mcp` 的 list/search/describe/call 上过滤**：`hidden` 任何调用方都看不到，
  `codemode` 系只对**脚本调用**可见。为此给 `ToolExecCtx` 加 `script_call: bool`，由
  `Extension::hosts_scripts()`（codemode 返回 true）在 `run_nested_tool` 构造子上下文时置位。
- `autoEnableCodemode`：`on_session_start` 时若存在 `codemode` 系服务器则自动启用 codemode 扩展
  （顶层显式 `false` 时不动作；不写回 settings）。
- CLI：`prux mcp add|update --exposure <mode>`；`/mcp status` 每行输出 `exposure=…` 与逐工具覆盖差异。

### 20.6 TUI：嵌套调用渲染（2.1 余项的 TUI 卡片）

对齐 pi「嵌套调用显示在父行内」：

- 结果态：`details.nestedCalls` → 工具块内渲染 `↳ <tool>  <参数摘要>  <耗时>`（失败为 `✗`）；
  参数摘要优先取 `command`，否则压缩 JSON，超 80 字符按字符截断。
- 进行中（实时）：`tool_execution_start/end` 事件带 `parentToolCallId` 时累积到
  `App::nested_calls`（父工具块内实时显示），`turn_end` 清理；`tool_started_at` 不再被嵌套调用污染。
- `ToolResultView`（`text` / `is_error` / `duration_ms` / `details`）取代原先的
  `(String, bool, Option<u64>)` 元组，渲染层才能拿到 `details`。

### 20.7 与 pi 的偏差（本轮新增/消解）

1. **grammar 的载体**：pi 用工具对象的 `constrainedSampling` 字段；prux 走 schema 的 `x-prux-grammar`
   （工具表是三元素元组）。语义等价，工具名 → 属性名的推断逻辑与 pi 相同（`inferGrammarInputProperty`）。
2. **grammar 无变体时退回 function 工具**（pi 抛错）：prux 选择宽容处理，避免一个扩展声明把整轮请求打挂。
3. **MCP 的 exposure 粒度**：pi 每个 MCP 工具都是独立工具、可单独设 exposure；prux 只有聚合的 `mcp` 工具，
   故服务器级 exposure 聚合成一个工具级 exposure，逐工具差异在 `mcp` 的 list/search/call 上按调用来源过滤。
4. **`codemode-deferred` 与 `codemode` 在 prux 等价**：prux 的 codemode 描述不列 MCP 工具清单，
   「不进描述」的差异无处体现。

> 原文的偏差 5（`autoEnableCodemode` 只改内存启用态）已删除：pi 同样只在会话内
> `setActiveTools` 激活 codemode 工具（`extensions/mcp/index.ts`），两边行为一致，不是偏差。

19.3 的偏差 2/3/5/7 已消解（`models.*`、grammar、实时嵌套进度 + TUI 渲染、bash 1 MiB 通道）；
6 也已消解（20.3 起失败结果同样带 `attachments`/`is_error`）；
偏差 1（无独立 worker）、4（`store()` 按整会话，理由已订正）、8（激活方式，用户拍板的刻意口径）保持。

### 20.8 验证

```bash
cargo test                    # lib 2200 passed / 0 failed / 1 ignored；全部集成测试通过
cargo clippy --all-targets    # 仅剩既有 warning（settings_manager.rs:266、tasks/widget.rs、examples/bot/*）
cargo fmt --check             # 通过
```

新增测试（本轮 30 个）：

| 测试 | 锁住的行为 |
|---|---|
| `tools::bash::collector_read_full_output_*`（3 个） | 未超限读全量、超限保留首尾、按字符边界切 |
| `tools::bash::bash_outcome_carries_structured_output` / `..._without_exit_code` | 结构化输出只在拿到退出码时生成 |
| `agent_session::codemode_script_receives_full_bash_output` | 脚本侧拿到 1 MiB 结构化 bash 结果 |
| `agent_session::codemode_script_reads_structured_error_result` | 带结构化输出的失败结果按结构化值进脚本（不抛） |
| `agent_session::codemode_script_runs_classifier_and_counts_usage` | 端到端：`models.classify` → 假分类器、usage 并入结果、`details.modelCalls` 记录 |
| `codemode::tests::{model_type_and_provider_arguments_are_validated, classifier_ref_uses_only_provider_and_id, catalog_lookups_return_model_info, available_models_require_credentials, description_includes_model_api}` | `models.*` 的参数校验、凭据过滤、目录查询与描述段 |
| `provider::completions::tests::{grammar_tool_conversion_emits_custom_tool, grammar_custom_input_is_wrapped_into_json_arguments}` | completions 的 `custom` 工具形状与裸输入包装 |
| `provider::responses::tests::{grammar_tools_convert_to_custom_items, custom_tool_call_input_is_wrapped_into_arguments, grammar_tool_call_replays_as_custom_items}` | responses 的工具形状、流式包装与历史回放（`ctc_` 前缀） |
| `agent_session::parameters_with_grammar_sampling_annotates_schema` / `codemode::tool_definition_shape` | schema 注入 `x-prux-grammar` 与 codemode 的 lark 声明 |
| `mcp::config::tests::{tool_exposure_resolution_prefers_exact_then_wildcard, codemode_exposure_is_detected, exposure_fields_parse_from_json}` | 逐工具 exposure 解析优先级、codemode 系识别、JSON 解析与非法值报错 |
| `mcp::tests::{aggregate_exposure_picks_most_visible, tool_visibility_follows_exposure_and_caller}` | 聚合取最可见、`hidden`/`codemode` 系的可视性与拒绝文案 |
| `cli::mcp_command::tests::exposure_flag_sets_server_exposure` | `--exposure` 写入配置、非法值报错 |
| `render::messages::tests::{nested_calls_from_details_render_inside_tool_block, live_nested_calls_render_while_tool_runs}` | 嵌套调用结果态/进行中的渲染 |
| `handlers::events::tests::nested_tool_events_accumulate_under_parent` | `parentToolCallId` 事件累积、结束标记、不进顶层计时表 |

改动文件：`src/core/tools/{bash,powershell,index}.rs`、`src/core/{agent_session,agent_loop,provider,model_resolver,extensions}.rs`、
`src/core/extensions/tools.rs`、`src/core/provider/{usage,completions,responses}.rs`、
`src/extensions/codemode.rs`、`src/extensions/codemode/{description,source}.rs`、
`src/extensions/mcp.rs`、`src/extensions/mcp/config.rs`、`src/cli/mcp_command.rs`、
`src/modes/interactive/{app.rs,handlers/events.rs}`、`src/modes/interactive/render/{messages,history}.rs`、本文档。
未提交 commit（按项目规则：未显式要求不自动提交）。

---

## 二十一、第十六轮：对照 pi CHANGELOG 复审出的三处缺口

第十五轮宣称「2.1 余项全清」后，按 pi 各包 `CHANGELOG.md` 的 `[0.99.0]` / `[0.99.1]`
逐条重审（不只信本文档的结论），发现三处**既未实现、也未标为刻意不做**的缺口，
另有两处陈旧断言在 HEAD 上本就是红的（与本轮改动无关，一并修掉）。

### 21.1 远程目录刷新缺 `types=chat,image,classifier`（coding-agent 0.99.0 Added）

pi：`remote-catalog-provider.ts` 请求 `/api/models/providers/{id}` 时带
`url.searchParams.set("types", REMOTE_CATALOG_MODEL_TYPES.join(","))`
（`REMOTE_CATALOG_MODEL_TYPES = ["chat", "image", "classifier"]`），
否则服务端只回 chat-only 分片。

prux `core/model_refresh.rs` 的 URL 只拼 provider id → **远程覆盖层永远是 chat-only**，
image / classifier 条目只能靠 `assets/models/` 静态基线，上游新增的图片/分类器模型
不会随刷新进来（静态目录同步过的条目仍可用，所以此前的功能验证没暴露它）。

**实现**：抽出 `catalog_url(provider)`（便于单测），URL 加
`?types=chat%2Cimage%2Cclassifier`（provider id 与类型列表都过 `urlencode`，
逗号编码成 `%2C`，与 pi 的 `URL.searchParams` 一致）；新增常量
`REMOTE_CATALOG_MODEL_TYPES`。

### 21.2 内置 dark/light 未换成 pi 的 OKHSL 调色板（coding-agent 0.99.0 Changed）

pi：`Changed the built-in dark and light themes to the revised pi colors, written in OKHSL`。
prux 第三轮把主题解析做到了支持 `oklch()`/`okhsl()`（`theme_json_supports_oklch_okhsl_and_short_hex`），
但**内置 dark/light 文件仍是旧 hex 调色板**（`accent #8abeb7`、`green #b5bd68`…），
而默认主题 `system` 在终端不上报配色时回落的正是这两份 → 新配色永远看不到。

**实现**：

1. `assets/themes/{dark,light}.json` 直接取 pi
   `packages/coding-agent/src/modes/interactive/theme/{dark,light}.json` 的原文
   （`vars` 用 `okhsl()`，新增 `blueBg`/`violet`/`scrollbarTrack`/`searchMatch*`，
   并带上显式 `appearance`）。
2. **消费端改用 pi 的规范键名（不做旧名兼容）**：pi 的主题 token 集就是规范名，
   而 prux 渲染/扩展此前还读一批自定义旧键。第一版曾加 `apply_legacy_aliases()` 别名层，
   随后**按用户拍板移除**，改为直接改用规范名（旧名一律不再认）：

   | 旧键（已废） | 现用规范键 |
   |---|---|
   | `userMsgBg` | `userMessageBg` |
   | `customMsgBg` | `customMessageBg` |
   | `mdMath` / `mdMathBlock` | `mdCodeBlock`（pi 无公式 token） |
   | `systemInfo` | `dim`（系统消息 info 级） |
   | `systemSuccess` / `systemWarning` / `systemError` | `success` / `warning` / `error` |
   | `dimGray` | `dim` |
   | `darkGray` | `borderMuted` |

   落点：`theme.rs::user_msg_style`、`messages.rs`（`SYS_LEVEL_KEYS` 从「候选键链」
   简化为单一规范键 + 兜底色）、`markdown.rs`（公式常量合并为 `MD_MATH` =
   `("mdCodeBlock", …)`）、`render/pending.rs`、`app.rs` 的文档注释；
   `load_system` 里那份手写别名也随之删除。`assets/themes/` 的 11 个额外主题里
   删掉已无人读取的 `system*` / `mdMath*` 键（各自都已定义规范键，删后行为不变）。
   `gray` / `cyan` 未被任何主题键查询消费（只出现在 subagent 的
   `NAMED_AGENT_COLORS` 色名表与 `vars` 引用里，属自由命名），因此保留。
3. **`/export` 的颜色转换**：`core/export_html/theme.rs` 新增 `to_css_color()`，
   把 `okhsl()`/`oklch()` 转 `#rrggbb`（CSS 认 `oklch()` 但不认 `okhsl()`，
   对齐 pi `theme.ts` 的「导出前把 okhsl 转 hex」），`colors` 段与 `export` 段都过一遍。

> pi 侧 `packages/coding-agent/src/modes/interactive/theme/` 经 grep 确认**零处**引用
> 上述旧名（`theme-schema.json` 的 token 集本就是规范名），无需修改。

### 21.3 未知 `type` 条目被当成 chat（ai 0.99.0 Added）

pi：`entries of unknown model types are dropped`（`isSupportedModelType`）。
prux 的 `ModelType::from_catalog` 把「无法识别的非空取值」也归一到 `Chat`
→ 上游将来引入 `type: embedding` 之类，会以 chat 身份混进 `/model` 选择器、
选中后报 `unsupported api type`，正是 13.1 修掉的那类坑。

**实现**：`ModelType::from_catalog` 改返回 `Option<Self>`——缺字段 / 非字符串 / 空串
仍是 `Some(Chat)`（旧目录与用户 `models.json` 兼容），无法识别的非空取值返回 `None`；
`provider_models()` 末尾 `retain` 掉这类条目，因此列表 / 查找 / 默认模型三条路径
口径一致（覆盖静态目录、`models-store.json` 与 `models.json` 用户层）。

### 21.4 顺带修掉的两类陈旧断言（HEAD 上本就红）

1. **`↳` 占位符重复空格**：`DEF_NESTED` 是 `"↳ "`（含尾随空格），而嵌套调用渲染用
   `format!("{mark} {name}")`（`✗` 无尾随空格）→ 实际输出 `↳  bash`，
   与 `nested_calls_from_details_render_inside_tool_block` /
   `live_nested_calls_render_while_tool_runs` 期望的 `↳ bash` 不符。
   改为 `DEF_NESTED = "↳"`（与 `DEF_FAILED` 同口径），三个使用点各自补分隔空格
   （`render/messages.rs` 折叠摘要、`render/pending.rs` 队列提示、
   `extensions/subagent/widget.rs` 子代理缩进）。
2. **硬编码旧调色板的断言**：`system_message_keeps_line_breaks_and_multicolor`、
   `sys_level_falls_back_to_generic_keys`（`messages.rs`）与
   `tests/misc.rs` 的两个导出用例断言了 `#b5bd68` / `#666666` / `#d4d4d4`，
   换成 pi 调色板后必然失败 → 改为比对**主题自身解析结果**或形态
   （hex + 明暗），不再硬编码调色板。

### 21.5 与 pi 的偏差（本轮新增）

1. **不做旧键兼容**（用户拍板）：主题 token 一律用 pi 的规范名，prux 自创的
   `system*` / `mdMath*` / `userMsgBg` / `customMsgBg` / `dimGray` / `darkGray` 全部停用。
   代价：仍按旧名写的用户主题会失去那几个颜色，回落到代码内兜底色（pi 格式主题
   本来就要求定义规范键，所以影响有限）。
2. **系统消息 info 级用 `dim`**：pi 没有 `systemInfo` 这类级别 token，prux 原来
   以 `systemInfo`（= 旧的 dimGray）表现最弱文本，现在直接取 `dim`；
   success/warning/error 三级同理改用 `success`/`warning`/`error`。
3. **公式渲染用 `mdCodeBlock`**：pi 无公式 token，行内/块级公式共用代码块色
   （旧别名层也是这个映射），因此 `MD_MATH` / `MD_MATH_BLOCK` 合并为一个常量。
4. **`gray` / `cyan` 保留**：它们不是主题键，而是 subagent 的色名表
   （`NAMED_AGENT_COLORS`，对应 pi `resolveAgentColor`）与主题 `vars` 的自由命名，
   与 token 规范名无关。

### 21.6 验证

```bash
cargo test                    # lib 2206 passed / 0 failed / 1 ignored；全部集成测试通过
cargo clippy --all-targets    # 仅剩既有 warning（settings_manager.rs:266、tasks/widget.rs、examples/bot/*）
cargo fmt --check             # 通过
```

> 修前 HEAD 上 `cargo test --lib` 为 2201 passed / 2 failed（`↳` 占位符两处）；
> 换新调色板后又暴露 4 处硬编码调色板的断言（`messages.rs` 2 个用例、`tests/misc.rs` 2 个用例），
> 一并改为比对主题解析结果。现全绿。

新增/更新的测试：

| 测试 | 锁住的行为 |
|---|---|
| `model_refresh::tests::catalog_url_requests_every_model_type` | URL 带 `types=chat%2Cimage%2Cclassifier`；provider id 与类型列表都做百分号编码 |
| `model_resolver::tests::missing_catalog_type_defaults_to_chat`（改写） | 缺字段 / 非字符串 / 空串 → `Some(Chat)`；未知非空取值 → `None`；大小写与空白同 `parse` 口径 |
| `model_resolver::tests::unknown_catalog_type_entries_are_dropped` | `type: embedding` 的条目不进 `list_models`、`find_model` 也查不到，缺 `type` 的条目仍是 chat |
| `theme::tests::builtin_themes_use_canonical_token_names` | 内置 dark/light 上渲染消费的 21 个规范键都能解析成具体颜色；且不再定义 10 个旧键 |
| `theme::tests::self_referencing_colors_resolve_to_vars_color`（改写） | 自引用键解析成具体颜色（hex 或 `okhsl()`），不是键名、也不是终端默认色 |
| `messages.rs::tests::{system_message_keeps_line_breaks_and_multicolor, sys_level_uses_canonical_state_keys}`（改写） | 级别→规范键（info=dim / success / warning / error），主题缺键时用兜底色；配色不再硬编码调色板 |
| `tests/misc.rs::{export_theme_vars_resolve_refs_and_are_valid_css, export_html_contains_resolved_text_color}`（改写） | 导出 CSS 的 `--text` 是 hex 且为浅色（`okhsl()` 已被转成 hex） |

改动文件：`src/core/model_refresh.rs`、`src/core/model_resolver.rs`、`src/core/provider.rs`、
`src/core/export_html/theme.rs`、`src/modes/interactive/theme.rs`、
`src/modes/interactive/app.rs`、
`src/modes/interactive/render/{messages,pending,markdown}.rs`、`src/utils/glyphs.rs`、
`src/extensions/subagent/widget.rs`、`assets/themes/*.json`（dark/light 换 pi 原文 +
11 个额外主题清旧键）、`assets/docs/themes.md`、`tests/misc.rs`、本文档。
未提交 commit（按项目规则：未显式要求不自动提交）。

---

## 二十二、第十七轮：复审出的 4 项非刻意缺口（2.1 渲染 / 2.10 API / 2.7 项目级 / 1.11 性能）

按 pi 各包 `CHANGELOG.md` 的 `[0.99.0]`/`[0.99.1]` 段第三次逐条重审（不只信本文档结论），
发现 4 项**既未实现、也未在文档里标为刻意不做**的缺口，本轮一次做完。
其余同批复核项（`/settings` 子菜单鼠标后键盘丢失、`visibleWidth()`/`Box` 渲染缓存、
`mixColors`/`styleText`/`getTerminalColorMode` 等 TUI 库 API、`setWheelScrollLines()`）
经代码核对属结构性不适用或已被等价物覆盖，不在本轮改动内。
（第十九轮复核确认成立：prux 的设置选择器无鼠标处理且无 `Submenu` 项；
`utils/display.rs::display_width` 本就逐字符、不做 grapheme 分割；
`setWheelScrollLines()` 等价于 `/settings` 写入后 `wheel_accel.set_lines()` 即时生效。）

### 22.1 codemode 结果的模型调用视图（2.1 余项的渲染面）

pi `extensions/codemode/renderer.ts`：结果里逐条列出每个子调用（`…`/`✓`/`✗`/`⊘` +
名称 + 参数 + 耗时 + cost），折叠时只留最后 `CALL_PREVIEW_COUNT = 8` 条并给省略提示，
带成本的调用多于 1 条时追加 `Model calls: $x` 合计（合计覆盖全部调用）。

prux 此前只把 `models.classify()` 的调用记进 `details.modelCalls`（含 cost），
渲染层只消费 `details.nestedCalls` → 分类器调用的成本在 TUI 里看不见。

**实现**（`render/messages.rs`）：

- `NestedCallView` 新增 `cost: Option<f64>`（仅脚本内 `models.*` 调用上报）；
  `nested_calls_from_details` 改名 `sub_calls_from_details`：先读 `nestedCalls`，
  再追加 `modelCalls`（`status = running` 视作结果未到达，无耗时/成本）。
- 渲染函数 `render_nested_calls` → `render_sub_calls`，形态对齐 pi：
  `<状态图标> name  args  duration  cost`（图标 `…`/`✓`/`✗`，名称 muted、
  耗时与成本 dim）；折叠到末尾 `SUB_CALL_PREVIEW_COUNT = 8` 条 + 省略提示；
  `priced.len() > 1` 时追加 `Model calls: <合计>`。
- 新增 `format_cost` / `two_significant_digits`：对齐 pi `formatCost`
  （≥ $0.01 保留两位小数，更小取 2 位有效数字，即 JS `toPrecision(2)` 的定点/指数切换口径）。
- 实时（进行中）路径同样改走 `render_sub_calls`。

> 偏差：pi 把嵌套调用与模型调用合在一个 `details.calls` 列表里按时间穿插；prux 两个
> 键分开记录，因此分两块显示（嵌套在前、模型调用在后），无法还原二者的交错顺序。

### 22.2 跨 provider 的模型集合 API（2.10 的 API 面）

pi ai 0.99.0 新增 `Models.getAllModels()` / `getAllAvailable()`（后者只含有凭据的 provider）。
prux 只有逐 provider 的 `list_models` / `list_models_of_type` / `find_model*`。

**实现**（`core/model_resolver.rs`）：

- `CatalogModel { provider, id, name, kind }`：跨 provider 展开的轻量条目。
- `catalog_providers()`：内置 `SUPPORTED_PROVIDERS` + `models.json` 声明的自定义 provider。
- `list_all_models()` / `list_all_models_of_type(kind)`（对齐 `getAllModels` / `getModelsOfType`）。
- `list_all_available_models()` / `list_all_available_models_of_type(kind)`（对齐
  `getAllAvailable` / `getAvailableOfType`，provider 源为 `auth::list_configured_providers`）。
- 文档：`assets/docs/sdk.md` 新增「模型目录」小节。

> 偏差：pi 返回完整 model 对象；prux 返回轻量条目，取详情再
> `find_model_of_type(provider, id, kind)` → `model_config_from_entry(...)`。

### 22.3 按项目禁用/启用扩展（2.7 的 Built-in 段语义）

pi 0.99.0 的 `pi config` Built-in 段可**全局或按项目**禁用 `mcp`/`llama.cpp`/`codemode`/`tool-search`
（存为 `extensions` 设置里的 `-builtin:<name>`）。prux 的 `disabledExtensions` 此前只读全局
`settings.json`（`project_settings_value()` 只被 `defaultTools` 与 `settings_section` 使用）。

**实现**（`core/settings_manager.rs`）：

- `read_disabled_extensions()` / `read_enabled_extensions()` 改为**全局 ∪ 项目**（项目条目追加在后、
  同名去重）；未受信任的项目不参与（`project_settings_value()` 仍返回 Null）。
- `/extension` 面板与 `--no-extensions` 的写入仍固定写全局（`write_disabled_extensions` /
  `write_extension_states` 文档已注明）；项目级禁用/启用靠手写项目 `.prux/settings.json`。
- 文档：`assets/docs/{settings,extensions}.md` 更新「项目级只覆盖 defaultTools」的旧说明。

> 偏差：pi 在 `pi config` 里可选写入哪一层；prux 的面板没有作用域选择，项目层只读不写。

### 22.4 Markdown 解析结果跨主题/宽度复用（1.11 的第三项）

pi tui 0.99.0：`Markdown` reuses parsed tokens across theme and width changes。
prux 的 `render/history.rs::ensure_cache_params` 在宽度/主题变化时 `msg_cache.clear()`，
下一帧全部消息重渲染（含 markdown 重新解析）。

**实现**（`render/markdown.rs`）：

- 新增 `thread_local` 解析缓存 `PARSE_CACHE`：键为 `(tab 展开后的原文, math 开关)`，
  值为 `Rc<Vec<Event<'static>>>`（`Event::into_static()`），命中时逐字节校验原文
  （避免哈希碰撞取到别的文档）并提到队尾；上限 `PARSE_CACHE_ENTRIES = 64` 条 /
  `PARSE_CACHE_BYTES = 4 MiB`，超出从最旧条目淘汰。
- 事件源抽象：`trait EventSource<'a>` + `SliceSource`（共享切片，主流程）+
  `RefSource`（引用列表，列表项段落——嵌套列表事件被就地渲染并剔除，无法用连续区间表达）。
  `next_event()` 的引用生命周期来自 `'a` 而非 `&mut self`，因此递归渲染零拷贝、
  不与事件源的可变借用冲突。`render_blocks`/`render_heading`/`render_code_block`/
  `collect_inline`/`render_blockquote`/`render_list`/`render_item_inlines`/`collect_table`
  的 `&mut VecDeque<Event>` 签名全部改为 `&mut dyn EventSource<'a>`（`VecDeque` 依赖移除）。

> 偏差与范围：pi 的缓存挂在组件上；prux 是模块级有界缓存（按文本键）。复用的只有
> **解析**（pulldown 事件序列）；主题/宽度相关的**布局**（颜色解析、syntect 高亮、折行）
> 仍按当前主题/宽度重做——这与 pi 的语义一致。

### 22.5 验证

```bash
cargo test        # lib 2213 passed / 0 failed / 1 ignored；全部集成测试通过
cargo clippy --all-targets   # 仅剩既有 warning（settings_manager.rs:266、tasks/widget.rs、examples/bot/*）
cargo fmt --check # 通过
```

新增测试：

| 测试 | 锁住的行为 |
|---|---|
| `render::messages::tests::model_calls_from_details_render_with_cost_and_total` | 逐条渲染模型调用（`✓ name  args  duration  cost` / 失败 `✗`）与 `Model calls: $x` 合计 |
| `render::messages::tests::model_calls_fold_to_preview_and_total_covers_all` | 折叠到末尾 8 条 + `... (N earlier calls, ...)` 提示；合计覆盖全部调用 |
| `render::messages::tests::format_cost_matches_pi` | `formatCost` 口径（`$0.0012` / `$0.0020` / `$0.01` / `$1.50` / `$0.0000012` / `$1.2e-7`） |
| `render::messages::tests::{nested_calls_from_details_render_inside_tool_block, live_nested_calls_render_while_tool_runs}`（改写） | 子调用行改为 pi 的状态图标形态（`✓` / `✗` / 进行中 `…`） |
| `model_resolver::tests::cross_provider_model_collections_cover_types_and_providers` | 全量视图跨 provider/类型；指定类型视图与类型子集一致；available 是全量的子集 |
| `settings_manager::tests::extension_state_lists_merge_project_layer` | 项目层并集（去重、追加在后）；未受信任项目不参与合并 |
| `markdown::tests::parse_cache_reuses_events_across_theme_and_width` | 同文本命中同一 `Rc`（宽度变化仍复用）；math 开关不同不共用条目 |
| `markdown::tests::parse_cache_evicts_oldest_entry` | 超过 `PARSE_CACHE_ENTRIES` 时淘汰最旧条目 |

改动文件：`src/modes/interactive/render/messages.rs`、`src/modes/interactive/render/markdown.rs`、
`src/modes/interactive/handlers/events.rs`、`src/core/model_resolver.rs`、
`src/core/settings_manager.rs`、`assets/docs/{sdk,settings,extensions}.md`、本文档。
未提交 commit（按项目规则：未显式要求不自动提交）。

---

## 二十三、第十八轮：剩余四项收尾（typesafe / 扩展协议实现 / 数组式目录）

第一节～二十二节之外，本轮按用户指示收尾 17 节复审列出的剩余缺口里的三项
（原第 4 项「`/proxy` 网关路由虚拟模型」用户指示**不做**）：

1. **`typesafe` provider 缺失**（第六节复审列出的第 1 项）；
2. **扩展不能注册 image / classifier 模型与自定义 API 实现**（第 2 项）；
3. **`models.all.json` 数组式目录未采用**（第 6 项）。

### 23.1 `typesafe` provider（对齐 pi 的内置 TypeSafe provider）

pi 的 `typesafeProvider()`（`ai/src/providers/typesafe.ts`）：`TYPESAFE_API_KEY` 鉴权、
`baseUrl = https://api.typesafe.ai/v1/`、目录里只有 classifier 模型 `jev-latest`
（`api = typesafe-system-one`，`contextWindow = 64000`，单价全 0）。

- `model_resolver::SUPPORTED_PROVIDERS` 加 `typesafe`；
- `auth::api_key_env_var` 加 `typesafe => TYPESAFE_API_KEY`；
- `login_registry::LOGIN_PROVIDERS` 加 `TypeSafe`（API key 登录，非订阅）；
- 目录条目随数组式目录同步进来（`providers/typesafe.all.json`）。

该 provider 没有 chat 条目：`list_models` 为空、`default_model_for` 为 None（不再需要
「目录第一个条目」兜底），分类器经 `find_model_of_type(.., Classifier)` 取用。

### 23.2 扩展注册 image / classifier 模型与协议实现（口径 B）

对齐 pi 的 `createProvider({ models, images, classifiers })`：扩展既能提供**目录条目**
（含 image / classifier），也能提供**协议实现**。

- 新增 `core/provider/api_impls.rs`：`ImageApiImpl` / `ClassifierApiImpl` trait +
  按 `api` 名索引的注册表（`RegisteredImageApi` / `RegisteredClassifierApi`），
  归属扩展的实现随扩展启停过滤（无归属 = SDK 直注册，恒可用）；
- `Extension` trait 新增 `image_apis()` / `classifier_apis()`；`register_extension_arc` /
  `unregister_extension` 同步登记与注销（与 `virtual_models` 同款生命周期）；
- `provider::generate_images` / `classify` 分发改为：**扩展注册的实现 → 内置实现
  （`openrouter-images` / `typesafe-system-one`）**，都没有时才报「协议未实现」；
  “失败永不抛错”的契约不变；
- `VirtualModelDefinition` 加 `model_type` / `api` / `base_url` / `cost` / `output`，
  `router` 改成 `Option`：新增 `VirtualModelDefinition::operation(provider, id, name, type, api)`
  构造 image / classifier 目录条目（无路由器，不参与路由）；
  `resolve_route` 对非 chat 型直接报 “is not a chat model”；
- `model_resolver`：虚拟条目按**自己的类型**并入对应列表（`list_for_provider_of_type`）、
  `find_model_impl` 按类型命中；`virtual_model_entry` 透传 api / baseUrl / cost / output。

### 23.3 数组式模型目录（完全按 pi 的发布布局）

pi `generate-models.ts` 的 `jsonOutputDir` 输出：键式 `models.json`（chat-only）+
数组式 `models.all.json`（含全部类型）+ `providers.json` + `providers/{id}.json` +
`providers/{id}.all.json`；`models.all.json` 里同一上游 ID 可按类型各有一条，
每个 provider 内顺序为 chat → image → classifier、同类内按 id 字典序。

- `scripts/sync-models.py` 重写为该布局（仍是同一个入口：`--check` / `--pi-ai-data`），
  并清理旧布局的 `models.<provider>.json`；本次已按 pi-ai 0.99.1 全量重跑
  （34 个 provider、71 个文件更新、33 个文件删除）；
- 运行期只内嵌 `assets/models/models.all.json`（`baseline_catalog()` 解析一次并缓存，
  避免 `list_all_models` 这类「逐个 provider 问一遍」的调用反复解析整份目录）；
- **三层的合并键改为 `(type, id)`**（对齐 pi `isModelType(model, "chat") && model.id === id`）：
  原先按 id 合并且后者胜，会让动态缓存里的 image 条目顶掉同 id 的 chat 条目；
- `models.json` 用户层逐条对齐 pi `applyModelsJson`：`baseUrl` 作用于全部类型、
  `compat` 只作用于 chat、`models` 只替换同 id 的 **chat** 条目、`modelOverrides` 只作用于 chat；
  缺省继承改用 pi 的 `findModelDefaults`（只看同类型条目：同 id → 同 api →
  `openai-completions` → 第一个），避免把 typesafe 这类「只有 classifier 条目」的
  provider 的 api / baseUrl 继承给用户新增的 chat 模型。

### 23.4 与 pi 的偏差（本轮新增）

1. **目录文件按 provider 拆分为 `assets/models/providers/`**（pi 的发布目录结构），
   但运行期只读聚合的 `models.all.json`：键式目录（`models.json` /
   `providers/{id}.json`）为与 pi 发布形态一致而生成，prux 自身不消费。
2. **扩展面是 Rust trait 而非 pi 的 provider 工厂**：pi 用 `createProvider(...)` 一次性
   给模型列表与各操作实现；prux 用 `virtual_models()`（条目）+ `image_apis()` /
   `classifier_apis()`（实现）两个声明，按 `api` 名关联。
3. **扩展注册的实现覆盖同名内置实现**（pi 是「扩展 model list 整体替换 provider 目录」的粒度）；
   prux 目录层仍走基线 + 动态 + 用户三层合并，扩展条目只是叠加。
4. **不采用 pi 的 `AnyModel` 统一类型**：prux 继续用 `Value` 承载目录条目 +
   `ModelType` 判型（既有形态，改动面最小）。
5. **`/proxy` 网关仍不路由虚拟模型**（16.6 第 6 条的取舍保留，用户指示本轮不做）。

### 23.5 验证

```bash
cargo test                    # lib 2218 passed / 0 failed / 1 ignored；全部 18 个集成测试二进制通过
cargo clippy --all-targets    # 仅剩既有 warning（settings_manager.rs:266、tasks/widget.rs、examples/bot/*）
cargo fmt --check             # 通过
scripts/sync-models.py --check  # 0 个文件需更新（与 pi-ai 0.99.1 一致）
```

新增测试：

| 测试 | 锁住的行为 |
|---|---|
| `model_resolver::tests::typesafe_provider_ships_the_jev_classifier` | typesafe 在 SUPPORTED_PROVIDERS；`jev-latest` 是 classifier 条目、api `typesafe-system-one`、baseUrl `https://api.typesafe.ai/v1/`；chat 列表为空、无默认模型 |
| `model_resolver::tests::store_and_user_models_do_not_clobber_other_types` | 动态缓存里的 image 条目不会顶掉同 id 的 chat 条目；`models.json` 新增同 id chat 模型时同样不顶掉 image，且 api 按同类型条目继承 |
| `provider::api_impls::tests::extension_image_api_is_dispatched_and_follows_extension_state` | 端到端：扩展注册的 image 条目进目录、`generate_images` 走扩展实现取到结果；扩展禁用后回落成「协议未实现」错误 |
| `provider::api_impls::tests::registered_apis_follow_owner_active_state` | 未注册/未启用扩展名下注册的实现不参与分发；未注册过的 api 名不可用 |
| `provider::api_impls::tests::standalone_impls_survive_owner_unregister` | 无归属（SDK 直注册）的实现不被 `unregister_owner` 清掉 |
| `auth::tests::api_key_env_var_mapping`（更新） | `typesafe` → `TYPESAFE_API_KEY` |
| `modes::interactive::handlers::auth::tests::login_flow_reaches_provider_panel_and_stays_open`（更新） | provider 面板 34 项 |

改动文件：新增 `src/core/provider/api_impls.rs`、`assets/models/{models.json,models.all.json,providers.json}`、
`assets/models/providers/*.all.json`（+ `*.json`）；删除 `assets/models/models.<provider>.json`（33 个）；
修改 `scripts/sync-models.py`、`src/core/model_resolver.rs`、`src/core/virtual_models.rs`、
`src/core/provider.rs`、`src/core/extensions.rs`、`src/core/extensions/registry.rs`、
`src/core/auth.rs`、`src/core/login_registry.rs`、
`assets/docs/{models,extensions,virtual-models,sdk,custom-provider}.md`、本文档。
未提交 commit（按项目规则：未显式要求不自动提交）。

---

## 二十四、第十九轮：复审「刻意不做 / 结构性不适用」的判定（文档轮）

第十八轮宣称剩余缺口已收尾后，按用户指示**专门复核被归入「刻意不做 / 结构性不适用」
的那些条目是否真的成立**（不看本文档的结论，逐条回到 pi 源码 + prux 代码求证）。
本轮不改代码，只订正文档结论与新增待办；结论分四类：

### 24.1 判定不成立：真缺口（已移出「不适用」，进 2.14 待办）—— 两项均已在第二十轮落地

#### 24.1.1 macOS Finder 文件粘贴（pi #9999）—— 原判「不支持 macOS 剪贴板 → 不适用」

原结论错在把 prux 当成 Linux-only。事实：

- `utils/clipboard.rs` 只在 Linux 且 Wayland 下走 `wl-paste`；**其余平台（含 macOS）
  走 arboard**（`paste_from_clipboard` / `paste_image_from_clipboard`），
  `Cargo.toml` 只有 `[target.'cfg(unix)'.dependencies]`，`wheel.rs` / `tools_manager.rs`
  里都有 `target_os = "macos"` 分支 → macOS 是被支持的平台。
- 粘贴入口 `handlers/mouse.rs::paste_clipboard` 是**图片优先**：先 `read_clipboard_image()`
  写临时 PNG 并插入其路径，失败才插文本 —— 这正是 pi 修复前的判型顺序。
- 后果与 pi #9999 同形：macOS 上复制文件（Finder）时，剪贴板带文件图标/预览图，
  arboard 能取到 image → prux 插入一个临时 PNG 路径而不是文件路径。
- 修法（待定）：自建 file URL 读取（pi 用 `NativeClipboard.getFilePaths()`；prux 无该层，
  需要 macOS 专用实现或命令回退），并在 bash 模式下给插入的路径加引号
  （pi 同批修复的第二半）。
- 影响面仅 macOS，Linux 路径不受影响；因此此前所有 Linux 侧验证都发现不了它。

#### 24.1.2 `pi.registerMcpServer()`（0.99.0 MCP Added 的一部分）

pi 0.99.0 的 MCP 声明是「服务器来自 `mcp.json`（全局 / 受信任项目）**或**
`pi.registerMcpServer()`」，并提供 `pi.unregisterMcpServer()`：按会话作用域注册、不落盘、
`mcp.json` 同名优先、`/mcp` 显示来源与覆盖关系。

prux 侧：`Extension` trait 的完整方法清单里没有任何 MCP 注册/注销口
（`core/extensions.rs`），`extensions/mcp.rs` 只走配置解析（`config::add_server` 是
CLI 与 `mcp.json` 的写入路径），因此**扩展无法以会话为作用域注册 MCP 服务器**。
这与第十八轮刚补的 image / classifier API 注册面（`Extension::image_apis()` /
`classifier_apis()`）是同型能力，可沿同一套「归属扩展 + 随启停过滤」的模式补。

### 24.2 判定不成立：结论可保留，但理由写错

| 位置 | 原文（错） | 复核结论 |
|---|---|---|
| 一.13 · #9944 | 「prux 无 `/skill` 命令与 skill 补全（SuggestionKind 仅 …/History）→ 不适用」 | prux **有** `/skill:<name>` 候选与裸名匹配（`handlers/suggest.rs:361-370`，回归测试 `skill_commands_rank_by_bare_name`，对应 pi #9120 同型问题）→ 应记「已有等价实现」 |
| 一.13 · #10026 | 与 #8938 / #9999 合并成「不做终端图片渲染 + 不支持 macOS 剪贴板 → 不适用」 | #10026 是「扩展在退出时关覆盖层导致 shell 光标未恢复」，与图片无关；prux `teardown_terminal` **无条件** `show_cursor()` → 结构性不受影响（结论对、归组与理由错） |
| 8.6 取舍 2 | 「crossterm 不给这类事件，需要另造输入旁路」 | crossterm 确实没有 `CSI ? 997;n` 的 variant；但旁路不是“成本取舍”那么轻：crossterm 0.29 会把 `?` 开头的未知 CSI **留在解析缓冲里**（吞掉后续按键），而它直接从 fd 0 读、无注入解析口 → 只能自持 stdin 读端（pty 转发）。第二十轮试做后按用户指示移除，**该功能不做**（详见 8.6 取舍 2 与第二十五节） |
| 19.3 偏差 4 | 「扩展拿不到会话分支」 | `Session::get_leaf_id()` / `find_entries_on_branch()` 都在，`virtual_models::read_state` 已在用；真正缺的是 `ToolExecCtx` 没有 session 句柄（脚本执行期拿不到当前叶子）→ 属未接管线，不是结构性；**第二十轮 25.3 已接上**（`ToolExecCtx::session_branch_entries`） |
| 19.3 偏差 8 / 2.14 的 2.7 余项 | 「若需逐字对齐 pi，需让扩展工具也进 `defaultTools` 选择面」 | **用户拍板**：`defaultTools` 只处理内置工具；`codemode`/`mcp`/`tool_search`/`jet` 属扩展工具，由 `enabledExtensions`/`disabledExtensions` 与 `/extension` 面板控制。pi 的 `"+codemode"` 在 prux 里等价于「启用该扩展」→ 这是**刻意的口径差异**，不是缺口；§18.4 原写的「2.7 口径遗留，需补」已作废 |
| 一.13 · #9786 | 「理论风险低，**暂列 ✅**，遇到再修」 | 无实测依据就标 ✅：既非刻意不做也非结构性不适用；prux X11 同样先试 `arboard::get_image()` → 改标 **待验证**（第二十轮 25.1 给出结论：严格 PNG 解码使其不会误判，另加图片目标探测） |

### 24.3 判定不成立：结论已过期（代码早在后续轮次改掉）

| 位置 | 原文 | 实际 |
|---|---|---|
| 19.3 偏差 6（及 20.7 的“偏差 6 保持”） | 「失败结果不带图片附件（`ToolResult`/`ToolError` 二分支）」 | 第十五轮 20.3 起失败结果同样走带 `is_error` 的 `ToolResult`：`extensions/codemode.rs` 把脚本已产生的文本、`attachments` 与分类器用量一并回传 → **已消解** |
| 13.3 偏差 2 | 「`exposure = codemode` 当前等价 `Hidden`」 | 第十四轮 codemode 落地后由 `ToolExposure::is_script_callable` 区分（`Codemode => true`、`Hidden => false`）→ **已消解** |
| 20.7 偏差 5 | 「`autoEnableCodemode` 只改内存启用态（不写 `settings.json`），重启后仍由用户设置决定」 | pi 同样只在会话内 `setActiveTools` 激活 codemode 工具（`extensions/mcp/index.ts`）→ **两边一致，不是偏差**，该条已删 |
| 2.3「仍未落地」 | 「`isError` 作为独立于 `text` 的结构化结果字段」 | 第十五轮 20.3 已落地（`ToolResult.is_error`）→ 已改写为“已落地” |

### 24.4 判定成立（复核依据）

| 原判 | 复核依据 |
|---|---|
| Kitty 图形协议（#8938）不适用 | prux 无终端图片渲染，只有 `[image]` 占位（`render/messages.rs`） |
| npm/git 扩展包（#9863/#9982）不适用 | 扩展是编译期内置 Rust，无包加载器 |
| RPC（`prompt/steer/follow_up` disposition、`RpcClient`）与 `--mode` CLI 不适用 | prux 无 RPC 服务与 `--mode`（v0.87.1 文档已排除，本区间无新增） |
| Mistral / llama.cpp / cloudflare-workers-ai / Azure / Vertex / Bedrock 等 provider 排除 | `SUPPORTED_PROVIDERS` 34 项与 `assets/models/providers.json` 逐个核过，均不含 |
| `packages/durable` 整包不适用 | pi 的 `coding-agent/package.json` **不依赖** `pi-durable` → 对 coding agent 确无对应结构 |
| `packages/{client,protocol,server,telemetry}` 无条目 | 0.99.0/0.99.1 两段确实为空 |
| 三.1 / 三.2 的 pi 侧 breaking 不需同步 | prux 未用过 `ImagesModels` / `queryTerminalColors()` 等被移除的 API |
| `/settings` 子菜单鼠标后键盘丢失不适用 | prux 的设置选择器无鼠标处理、且“当前无 Submenu 项”（`settings_selector.rs`） |
| `visibleWidth()`/`Box` 缓存、`mixColors`/`styleText`/`getTerminalColorMode`、`setWheelScrollLines()` 不适用/已等价 | prux 不用 pi 的 TUI 库；`display_width` 本就逐字符、无 grapheme 分割；滚轮运行时改值经 `wheel_accel.set_lines()` 即时生效 |
| 19.3 偏差 1（无独立 worker 线程） | rquickjs 进程内无 `terminate()`，只能靠中断处理器 + 墙钟 + 256 MB 上限；属实际限制 |
| 16.6 偏差 6（`/proxy` 不路由虚拟模型）、23.4 偏差 5 | 用户指示本轮不做，保留 |

### 24.5 本轮改动

仅文档（`migrations/migration-v0.99.1.md`）：订正上文各条，2.14 增两行待办
（macOS Finder 粘贴、`pi.registerMcpServer()`）与一行待验证（#9786）。
不改代码，故无 `cargo` 验证；未提交 commit（按项目规则：未显式要求不自动提交）。

> 复核对齐的 pi 侧依据：`packages/coding-agent/CHANGELOG.md`（0.99.0/0.99.1 段）、
> `packages/ai/CHANGELOG.md`、`packages/tui/CHANGELOG.md`、
> `packages/coding-agent/src/core/{extensions,mcp-servers}.ts`、
> `packages/coding-agent/src/extensions/mcp/{index,config}.ts`、
> `packages/coding-agent/docs/{extensions,mcp}.md`、`packages/coding-agent/package.json`。

---

## 二十五、第二十轮：清掉第十九轮待办 + 2.5 的两项取舍

用户指定：A 组（真缺口）与 B 组（性价比高的偏差）**全做**，C 组（仓库自有待办）不做；
随后又指示「系统主题实时明暗切换不需做」。本轮因此包含 5 项落地 + 1 项试做后移除。

### 25.1 剪贴板：粘贴优先级 + 文件路径（#9999 / #9786）

- `utils/clipboard.rs`
  - 新增 `read_clipboard_file_paths() -> Vec<String>`：macOS / Windows 走 arboard 的
    `get().file_list()`（macOS 后端用 `NSPasteboard` 的 `NSPasteboardURLReadingFileURLsOnlyKey`
    读 file URL，等价于 pi 新增的 `NativeClipboard.getFilePaths()`，无需自写 ObjC）；
    Wayland 先试 `wl-paste -l` 探测 `text/uri-list` 再 `wl-paste -t text/uri-list`，
    用 `url::Url::to_file_path()` 解析（新增纯函数 `parse_file_uri_list`，有单测）；
    无文件列表 / 后端不可用一律返回空 Vec（不当错误：真正拿不到剪贴板时后面的文本分支会报）。
  - 新增 `clipboard_advertises_image() -> Option<bool>`：Linux 下用 `wl-paste -l`（Wayland）
    或 `xclip -selection clipboard -t TARGETS -o`（X11）先看剪贴板**自己声明了哪些目标**；
    `Some(false)` 时直接跳过图片读取。**#9786 的结论**：prux 的图片读取走 arboard，
    X11 上确实会请求未声明的 `image/png`（`arboard` 的 `read()` 不看 TARGETS），
    但 arboard 对字节做**严格 PNG 解码**，文本字节会解码失败并回落文本，
    因此不会复现 pi 的「文本被当成图片」；探测只是让这次请求根本不发生。
- `modes/interactive/handlers/mouse.rs::paste_clipboard`：优先级由「图片 → 文本」改为
  **文件路径 → 图片 → 文本**（与 pi 一致）。文件路径含控制字符时拒绝整次粘贴；
  bash 模式（输入以 `!` 开头）按 shell 规则加引号（安全字符集 `A-Za-z0-9_-./~:@`，其余单引号包裹并
  把 `'` 转义为 `'\''`）、多条以空格相连，其余模式每行一条；光标两侧都是非空白时补空格。
  抽成纯函数 `format_clipboard_paths` / `shell_quote_if_needed` + `Editor::cursor_neighbours()`，
  单测对齐 pi 的 `clipboard-paste-file-paths.test.ts` 五种场景。

### 25.2 扩展注册 MCP 服务器（`pi.registerMcpServer()` 等价物）

- `core/extensions.rs`：新增 `Extension::mcp_servers() -> Vec<DeclaredMcpServer>`（默认空）
  与收集器 `declared_mcp_servers()`（只收集**已启用**扩展、按注册顺序、同名先到先得）。
  条目是 `(服务器名, mcpServers 条目 JSON)`：核心不解释结构，由 MCP 扩展解析校验。
- `extensions/mcp/config.rs`：新增 `apply_declared_servers(&mut LoadedConfig, declared)`：
  校验名字与条目；**`mcp.json` 同名条目优先**（只追加一条来源标注，`/mcp` 据此显示覆盖）；
  非法条目跳过并把原因作为诊断返回。
- `extensions/mcp.rs`：扩展侧所有配置加载改走 `load_config_with_declarations()`
  （连接用的 `manager_for`、工具可见性 `server_entries`、启动提示、`autoEnableCodemode`、
  `enabled_servers`、`/mcp status|list`、`test|login|logout`）；CLI（`prux mcp …`）仍只读
  `mcp.json`，与 pi 的「`pi mcp` shell 命令只看到 mcp.json」一致。
  会话作用域天然成立：声明随扩展启停出现/消失，不落盘。

### 25.3 codemode `store()` 按当前分支（pi 语义）

- `core/extensions/tools.rs`：`ToolExecCtx` 新增 `session_branch_entries: Option<Arc<Vec<Value>>>`
  ——当前会话**活动分支**的原始条目（根在前）；`None` = 拿不到（无会话 / 非脚本宿主的调用 /
  子代理上下文），`Some(空)` = 该分支确实没有条目。
- `core/agent_session.rs`：`Agent::session_branch_entries()` 由 `Session::get_leaf_id()` +
  `Session::get_branch()` 求得；只在**脚本宿主工具**的调用上计算（`tool_hosts_scripts`，
  其它工具零开销），并沿嵌套调用继承。
- `extensions/codemode/store.rs`：`seed_from_session` → `seed_from_entries(&[Value])`
  （每次脚本开跑前按当前分支重建），另加 `pending_writes` 叠层：本进程内**尚未落盘**的写入
  （headless/SDK 无 UI 回执时）仍留在快照里，落盘后自动剔除。新增 5 个单测覆盖
  分支重放、未落盘写入/删除、空写入。
- `extensions/codemode.rs`：`run_script` 开跑前 `store::seed_from_entries(ctx…)`；
  去掉 `on_session_start` 的整会话播种（那是旧语义），保留 `on_session_switched` 的清空。

### 25.4 系统主题：索引兜底档（2.5 取舍 1 → 已实现）

- `modes/interactive/system_theme.rs`：`SystemThemeOutput` 新增 `dim`（需要 SGR 2 的 token），
  新增 `generate_indexed_theme_colors(saturation, appearance)` ——逐值对齐 pi 的 `indexedColors()`：
  面板类 token 不给颜色（终端默认、透明）、彩色 token 用色族 ANSI 槽位（`TOKEN_SLOTS` 可覆盖，
  写成 `ansi:<n>`）、中性色里正文以外的 token 记入 `dim`。
- `modes/interactive/theme.rs`：`Theme` 新增 `dim_keys`，`style()` / `style_fg_bg()` 对
  这些键叠加 `Modifier::DIM`；`load_system()` 在终端**未上报背景**时改用索引兜底档
  （不再回落内置 dark/light——终端会用自己的主题渲染这些索引，任何背景都贴合）。
- 断言更新：`system_theme_uses_indexed_tier_without_terminal_colors`、
  `indexed_tier_marks_neutral_tokens_faint`（原 `system_theme_falls_back_without_terminal_colors` 已作废）。

### 25.5 扩展可读主题色与外观（2.5 取舍 3 → 已实现）

- 新增 `core/theme_view.rs`：`ThemeView { name, appearance, colors, default_fg, default_bg }`
  + 进程级发布槽（`set` / `get`）+ `color()` / `is_light()` / `fg_ansi()` / `bg_ansi()`。
- `modes/interactive/theme.rs`：`Theme::view()` 把每个语义键解析成具体颜色
  （`ansi:<n>`、`oklch()` 都落到 `#rrggbb`；空值 = 终端默认色时用终端上报的默认前景、
  未上报则按外观猜黑/白），`Theme::publish_view()` 每帧发布（按 `render_fingerprint` 短路，
  主题切换后自动同步），在 `render_tick` 里调用。
- **消费者（不是空 API）**：`core/export_html/theme.rs` 在同名主题下优先用运行中 TUI 的视图
  ——`system` 这类**运行时推导**的主题磁盘上没有文件，按名加载只会得到内嵌 dark，
  `/export`、`/share` 导出的配色因此与用户实际看到的不一致；现在改为与 TUI 一致
  （CLI `--export` 无 TUI 时仍按名加载，行为不变）。

### 25.6 系统主题实时明暗切换（2.5 取舍 2）—— **试做后按用户指示移除**

先按 pi 做了完整实现（mode 2031 探测 + `CSI ? 997;n` 解析 + 自持 stdin 的 pty 输入旁路
`utils/tty_filter.rs`：真实终端读端由旁路线程持有，fd 0 换成 pty slave，摘掉报告后原样转发，
并同步 winsize/SIGWINCH、按需刷新配色），用户随后指示**不做**，已整体删除：
`utils/tty_filter.rs`、`nix` 的 `term`/`pty` feature、`terminal_colors` 里的
mode 2031 探测/开关/运行期刷新槽位、`theme.rs` 里的外观上报分支。

保留的结论（写进 8.6 取舍 2，供以后再评估）：

- crossterm 0.29 的解析器对**未知且以 `?` 开头的 CSI** 不丢弃而是留在缓冲区
  （`parse_csi` 的 `?` 分支只处理 `?u`/`?c`，其余 `Ok(None)` 继续等字节），
  于是 `CSI ? 997;n` 会吞掉之后的所有按键直到用户按下 `c`/`u`；
- crossterm 直接从 fd 0 读、且其 `EventSource` 是单例、无字节注入口，
  所以「只做查询不接管输入」在运行期不成立（其后台读线程会抢走回复）；
- 要接这个报告只能自持 stdin（pty 转发），代价与风险都不小（termios/winsize/嵌套程序）。
- 现状：外观只在启动时判定一次（上报背景 → COLORFGBG），与 pi 的差异如实保留。

### 25.7 验证

```bash
cargo test                   # lib 2244 passed / 0 failed / 1 ignored；集成测试全绿
cargo clippy --all-targets   # 仅剩既有 warning（examples/bot、settings_manager、tasks/widget）
cargo fmt --check            # 通过
```

未做真机验证的部分（如实记录）：Wayland/macOS 的「复制文件后粘贴」只覆盖了
`parse_file_uri_list` 纯函数与 arboard 路径的调用关系，未在真实剪贴板上跑过
（会覆盖用户当前剪贴板，故未做）；X11 的 `xclip -t TARGETS` 探测同理。

改动文件：`utils/clipboard.rs`、`modes/interactive/handlers/mouse.rs`、`modes/interactive/editor.rs`、
`core/extensions.rs`、`core/agent_session.rs`、`core/theme_view.rs`（新增）、`core.rs`、
`core/export_html/theme.rs`、`extensions/codemode.rs`、`extensions/codemode/store.rs`、
`extensions/mcp.rs`、`extensions/mcp/config.rs`、`modes/interactive/{theme,system_theme,interactive}.rs`、
`modes/interactive/render/messages.rs`（测试构造）。
未提交 commit（按项目规则：未显式要求不自动提交）。
