# Prux 迁移指南：pi 1.4.0 → 1.5.2

> **来源**：pi 仓库 tag `v1.4.0` → `v1.5.2`（含 `[1.5.0]` / `[1.5.1]` / `[1.5.2]` 三段）各包
> `packages/*/CHANGELOG.md`，逐条对照 prux 现码核对（引用点都写了 `文件:行号`）。
> prux 基线：pi **1.4.0**（上一份指南 `migrations/migration-v1.4.0.md` 已收口）。
> 数据面已先行完成：`assets/models/**` 已按上游同步（`make sync-models`），`Cargo.toml` 与
> `migrations/sync.md` 已记录 **1.5.2**，changelog 文件已改名为 `assets/changelog/v.1.5.2.md`。
>
> **收录规则**：只收录 ① prux 已实现功能的 bug 修复 ② 1.4.0→1.5.2 区间内 pi 新增/变更/移除的能力。
> prux 没有、且在本区间之前就已存在的 pi 能力（`packages/{chord,durable,env,telemetry,schema}`、
> Bedrock、Codex CLI、Radius、node/nix 打包面、`pi-schema` 校验层）不逐条列，第五节汇总。

**状态标记**：🔧 应修/应移植　⚠️ 需对照 pi 逐项核对（口径可能已偏差或需实测）　✅ 已满足/无此问题（附理由）
❌ 结构性不适用（prux 无对应实现）　🆕 本区间新增的候选移植项

**实施状态（第一轮 21 条 🔧、第二轮数据面与 6 条 ⚠️ 已落地；其余 ⚠️ 待核对、🆕 14 条未开工）**：

**第二轮（⚠️ 中已确认缺的 + 数据面）**：

- **数据面**：`default_model_id` 13 项已跟到 1.5.2；新增 `qwen-token-plan-individual-cn`
  （`SUPPORTED_PROVIDERS` / `login_registry` / `auth.rs` 的 `QWEN_TOKEN_PLAN_CN_API_KEY`），
  模型数据由 `scripts/sync-models.py` 的派生规则生成（`DERIVED_PROVIDERS`：中国目录 ∩
  Individual 白名单），不是手写 JSON。
- **1.15** 只做了闸门（超 64 KB 不再重解析中间增量），增量解析器未移植；
- **1.26 / 1.29 / 1.31 / 1.38 / 1.42** 已修；
- **1.25** 判定不改：prux 的 `simple_completion` 不发 effort 字段，不会撞 pi 那个 400；
- **1.28** 复核已满足：busy/steer 的消息经 `expand_at_image_refs` 缩放后才入队；
- **1.34** 未做（需要性能基线，块级缓存失效条件易出错）；
- 1.16–1.20、1.32–1.34、1.36–1.39、1.41 仍为「需实测」。

**第一轮（21 条 🔧）**：

| 提交 | 内容 |
|---|---|
| `3fa2112` | 1.7 / 1.8 / 1.10 / 1.11 / 1.12 / 1.13 / 1.14（provider 与协议层） |
| `4fcc0d7` | 1.21 / 1.22 / 1.23 / 1.24（信任、模板参数、模糊匹配、tool_search 分词） |
| `e9d9e04` | 1.1 / 1.2 / 1.3 / 1.4 / 1.5 / 1.6（grep / find / read / bash） |
| `1238c60` | 1.27 / 1.30 / 1.35 / 1.40（导出、Markdown、终端识别、MCP OAuth） |

上表的哈希在本仓 `git log` 里查不到（当初只记了分组，没记真实提交）；判断落地情况以代码为准。
各条的现状描述保留为「实施前」的记录；复核对齐时看「建议」是否已按其口径落地。

---

## 零、总体判断

| 分类 | 条目 | 数量 |
|---|---|---|
| 🔧 应修（工具/协议/信任，改动明确、收益直接） | 1.1–1.8、1.10–1.14、1.21–1.24、1.27、1.30、1.35、1.40 | 21 |
| ⚠️ 需核对（口径/实测依赖） | 1.9、1.15–1.20、1.25、1.26、1.28、1.29、1.31–1.34、1.36–1.39、1.41、1.42 | 21 |
| 🆕 新增能力（候选移植） | 2.1–2.14 | 14 |
| ✅ 已满足 / ❌ 不适用 | 第五节整表 | — |
| 数据面（已 sync，剩代码侧默认表） | 2.15、第四节 | — |

**建议实施顺序**（按性价比）：

1. **P0 一行级修复**：1.8（Claude Code 版本号）、1.10（Gemini `const`）、1.11（`session-id` 头）、
   1.13/1.14（重试与溢出表补串）、1.21（信任决策被自动信任覆盖）、1.23（模糊排序）、1.24（tool_search 分词）。
2. **P1 工具语义**：1.1（grep `glob` 基准）、1.3（limit 提示误报）、1.7（Anthropic `$defs`）、
   1.2/1.5（grep `context` 与 read 不再整文件入内存）。
3. **P2 协议正确性**：1.12（`requiresNonNullAssistantContent`）、1.4（find Windows 通配）、
   1.6（bash 超时退出码）、1.22（模板参数引号）。
4. **P3 性能**：1.15（流式工具参数增量解析，改动最大、只在超大工具调用上可见）、1.34（Markdown 块级缓存）。
5. **P4 新能力**：2.1（Anthropic message threads）、2.9（分类器）、2.10（compat 字段）、
   2.12（provider 计费）、2.13（会话目录消失时 Fork）。
6. **数据面**：先改第四节默认模型表，再动 2.15 的目录增补。

---

## 一、Bug 修复（prux 已有功能，升级时应评估移植）

### A. 工具层

#### 1.1 🔧 grep 的 `glob` 相对「启动目录」而非 `path`

**pi 1.5.1 Fixed**（#11807 / #10998）：`path: "repo"` + `glob: "migrations/main/*.js"` 返回
`No matches found`；现在 glob 相对 `path`，前导 `./` 被忽略。

**prux 现状**：`src/core/tools/grep.rs:95` 把 `glob` 直接塞给 `rg --glob`，而 `rg` 子进程
（`:105` 起）**没有 `.current_dir(...)`**，所以 glob 相对 prux 的 cwd，与 `path` 无关——
同一个 bug。`./src/*.ts` 也会因为 `./` 前缀匹配不到。

**建议**：`glob` 先按 `search_path` 相对化（`path.join(glob)` 的规范化形式）再传，
或把子进程 `current_dir` 设到 `search_path` 并把结果路径相对回 cwd；前导 `./` 直接剥掉。
补一条「path 是子目录、glob 带子目录」的用例。

#### 1.2 🔧 grep 带 `context` 时把整个文件读进内存，孤立 CR 也会错位

**pi 1.5.1 Fixed**（#9276 / #11423 / #11398）：`context` 改为由 ripgrep 自己产出
（`-A/-B/-C`），既不吃内存，也修了 UTF-16 与孤立 `\r` 导致的上下文行错位。

**prux 现状**：`src/core/tools/grep.rs:227` 对**每个命中**调用 `read_file_lines`（`:280`）
`tokio::fs::read_to_string` 整文件读入——大文件大内存；`\r` 在切分前被整体替换掉
（`:283-286`），所以「同一物理行的行号」与 rg 报的行号在孤立 `\r` 的文件上会错位。

**建议**：`context > 0` 时给 `rg` 加 `-C N`，按 `type=context` 事件消费上下文行（顺带不再
需要 `read_file_lines`）。这条同时消掉 1.3 的一半代码面。

#### 1.3 🔧 恰好 `limit` 条命中时仍报「limit reached」

**pi 1.5.1 Fixed**（#11423 / #7121）：`limit` 与结果数**相等**时不再提示；
`limit < 1` 一律当 1。

**prux 现状**：`src/core/tools/grep.rs:173` 在 `match_count >= effective_limit` 时就置
`match_limit_reached`——即便 rg 没有任何后续命中；`src/core/tools/find.rs:146` 是
`relativized.len() >= effective_limit`，同理。`limit < 1` 两边都已 `.max(1)`（`find.rs:46`、`ls.rs:41`，✅ 那半已对齐）。

**建议**：只有「确实还存在更多命中」时才提示（grep 侧看 rg 是否还有未消费的 match 事件，
find 侧看是否严格大于 limit）。补「恰好 N 条」与「N+1 条」两条用例。

#### 1.4 🔧 find 在 Windows 上对 `src\**\*.ts` 与 `src/**/*.ts` 匹配错误

**pi 1.5.1 Fixed**（#9262）：Windows 分隔符与所有 fd 版本行为归一；`src/**/*.ts` 也匹配
`src/` 直接下的文件，`src/*.ts` 不再匹配子目录。

**prux 现状**：`src/core/tools/find.rs:70-78` 用 `pattern.contains('/')` 判断「是不是路径模式」，
`src\**\*.ts` 走不到 `--full-path`；`src/**/*.ts` 被改写成 `**/src/**/*.ts`，仍然要求
`src/` 下**至少一级子目录**，与 pi 修好后的语义不同。

**建议**：分隔符先统一成 `/`；`**/` 的补写与「`**` 匹配零或多级」语义按 fd 的 glob 规则
重新对齐（等价于把 `a/**/b` 展成 `a/b` 与 `a/**/b` 两条 glob，或改用 `--and` 组合）。

#### 1.5 🔧 read 一次把整文件读进内存

**pi 1.5.1 Fixed**（#9654）：`offset`/`limit` 已给出时仍然整文件读入 → 大文件 OOM；
现在流式读，只保留要展示的行。

**prux 现状**：`src/core/tools/read.rs:85` `ops.read_file(&abs)` 一次读满再切行（`:101-103`）。
图片判定（`:95`）也依赖全量字节，所以「先读全量」是结构性的。

**建议**：`ToolOperations` 加一个可选的流式读入口，文本路径按 `offset`/`limit` 只读需要
的行；图片路径保留全量（有大小上限）。注意 `read_file` 是扩展可注入的接缝，新增入口要有
默认实现，别破坏既有 `ReadOperations` 语义。

#### 1.6 🔧 bash 超时只报「Command timed out」，丢掉已产生的退出码

**pi 1.5.1 Fixed**（#11613）：命令其实已退出、但后台子进程还占着管道导致超时时，
错误文案现在也带上 shell 的退出码与退出时间。

**prux 现状**：`src/core/tools/bash.rs:434-487`。`child.wait()` 被 `tokio::time::timeout`
包住，超时分支直接 `kill_process_tree` 并把 `status` 置 `None`，于是 `exit_code` 为空、
只打印 `Command timed out after N seconds`（`:481`）。另外 `_ = read_task.await`
（`:443`）在 `child.wait()` 之后**不受超时约束**：若 shell 已退出而后台子进程持续写管道，
这里会一直等到管道关闭（真正的挂死风险）。

**建议**：把「等退出」与「等管道收尾」都放进同一个超时窗口；超时时若已拿到 `status`，
就按 pi 的文案把退出码与结束时间一起报出来。

### B. provider / 协议层

#### 1.7 🔧 Anthropic 工具 schema 丢根 `$defs`，属性里的 `$ref` 悬空

**pi 1.5.1 Fixed**（#9557）：非 strict 工具 schema 只留 `properties`/`required` 时把根
`$defs`/`definitions` 一起丢了；Claude 于是给出错误参数（MCP 工具常见），部分兼容网关直接 400。

**prux 现状**：`src/core/provider/anthropic.rs:660-682` 正是「只取 `properties` + `required`」：

```rust
"input_schema": { "type": "object", "properties": properties, "required": required }
```

任何带 `$defs` 的 MCP 工具（`additionalProperties`、嵌套模型）都会断链。

**建议**：`input_schema` 保留 `$defs`/`definitions`/`additionalProperties`（即「除少数
Anthropic 不认的键以外原样透传」，把 `$schema` 之类剔除即可），补一条「属性 `$ref` 指向
根 `$defs`」的序列化用例。

#### 1.8 🔧 Anthropic OAuth 请求上报的 Claude Code 版本已过期

**pi 1.5.2 Fixed**：`claude-cli/<version>` 的 user-agent 落后于真实 Claude Code 版本。

**prux 现状**：`src/core/provider/anthropic.rs:55` `CLAUDE_CODE_VERSION = "2.1.280"`，
pi 现在是 **`2.1.322`**（`packages/ai/src/api/anthropic-messages.ts:109`）。

**建议**：改常量。这类值建议半年回查一次上游。

#### 1.9 ⚠️ Claude 账号把 effort 上限压到 `high` 以下时报 `Invalid effort level`

**pi 1.5.0 Fixed**（#10063）：组织级限额低于 `high` 时，pi 改为发固定的顶层 `low` effort。

**prux 现状**：`src/core/provider/anthropic.rs:957-967` 直接把 thinking 级别映射成
`output_config.effort` 发出去，没有「组织限额」兜底；被限额的账号在 `low`/`medium` 也可能被拒。

**建议**：照 pi 的做法在请求里补一个固定 `low` 的顶层 effort（细节以 pi 的
`anthropic-messages.ts` 为准），或至少实测一次被限额账号。

#### 1.10 🔧 Gemini 工具 schema 里的 `const` 被拒

**pi 1.5.0 Fixed**（#11308）：`const` 转成单值 `enum`，且不改写 `default`/`examples`/`enum`
里的字面量、不丢 `__proto__` 这类属性名。

**prux 现状**：`src/core/provider/google.rs:518-530` `convert_google_tools` 把
`parametersJsonSchema` **原样**下发，没有任何 schema 清洗——任何带 `const` 的工具在 Gemini 上
都会 400（prux 自己的 `read` 的 structuredOutput 就有 `{"type": {"const": "image"}}`，
`src/core/tools/read.rs:254`；MCP 工具带 `const` 也很常见）。

**建议**：加一层 Gemini 专用 schema 清洗：`const: v` → `enum: [v]`；同时按 Gemini 的限制
剔除不支持的键（`$defs`/`$ref`/`additionalProperties` 需要实测）。这是 prux 目前
**唯一一处完全不设防**的 provider schema 转换。

#### 1.11 🔧 OpenAI Responses 的会话亲和头用了 `session_id`

**pi 1.5.1 Fixed**（#11639）：文档写的是 `session-id`，pi 一直发 `session_id`。

**prux 现状**：`src/core/provider.rs:726-770` `session_affinity_headers` 对
`api == "openai-responses"` 的模型（`enabled` 恒为真）走 `format == "openai"` 分支，于是发
`session_id` + `x-client-request-id`，**没有** `session-id`。

**建议**：responses 走 `session-id`（completions 保持 `session_id` 不变），并把这里已有的
单测（`provider.rs:1862` 起）改成按 api 分档断言。

#### 1.12 🔧 只有工具调用的 assistant 消息硬编码 `content: null`

**pi 1.5.1 Added/Fixed**（#8118）：新增 `compat.requiresNonNullAssistantContent`，
给要求空串而非 `null` 的兼容端点用。

**prux 现状**：`src/core/provider/completions.rs:183` `assistant_msg.insert("content", Value::Null)`，
没有开关；被这类端点（常见于自建网关）直接 400。

**建议**：加 `compat.requiresNonNullAssistantContent`（目录 + `models.json` 双来源），
为真时写 `""`。

#### 1.13 🔧 重试表 / 不可重试表各缺几条

**pi 1.5.0/1.5.1 Fixed**（#11879 / #11537 等）：

| 类型 | pi 新增串 | 场景 |
|---|---|---|
| 可重试 | `cut short after the reply had begun` | 网关转发的 Anthropic 流中途被截断 |
| 可重试 | `subscription_sharing_usage_unavailable` / `subscription_sharing_user_unavailable` | Sign in with ChatGPT 用量/用户数据瞬时不可用 |
| 可重试 | `socket connection was closed` | 传输层主动关闭 |
| 不可重试 | `"code": "1308"`（正则，须带引号且后面不是数字） | Zhipu/Z.ai 套餐用量上限，以 429 下发 |
| 不可重试 | `available balance` | OpenCode Go 套餐限额文案 |

**prux 现状**：`src/core/agent_session.rs:2388-2448`（`RETRYABLE`）与 `:2367-2381`
（`NON_RETRYABLE`）——上述 5 条**一条都没有**。而 Zhipu 的 1308 会被状态码判定
（`contains_http_status_code`，`:2464`）当成 429 反复退避重试，永远不可能成功。

**建议**：`NON_RETRYABLE` 是纯子串表，1308 需要一小段手写判定（形如 `"code": 1308`
且数字后不是数字）——放进 `is_retryable_assistant_error` 的「先判不可重试」分支即可。

#### 1.14 🔧 LiteLLM 的 `ContextWindowExceededError` 不算上下文溢出

**pi 1.5.0 Fixed**（#11492）：加 `/context window exceeded/i`。

**prux 现状**：`src/core/agent_session.rs:3214-3268` 的 `OVERFLOW` 表有
`context window exceeds limit`（MiniMax 口径）与 `context length exceeded`，**没有**
`context window exceeded`——LiteLLM 前置检查的文案差一个 `s`/`ed` 就漏掉，
会话会卡死在超大请求上而不是压缩重试。

**建议**：补 `"context window exceeded"`，并补一条用例到
`overflow_error_patterns_cover_providers`。

#### 1.15 ⚠️ 流式工具参数每个 delta 全量重解析（O(n²)）

> **第二轮**：只落地了闸门（`json_args::streaming_arguments`，超 64 KB 的中间增量不再重解析），
> 增量解析器本身未移植。

**pi 1.5.0 Fixed**（#9265）：新增增量容错 JSON 解析器 `StreamingJsonParser` /
`ToolCallArgumentsStream`，每个 delta 只读一次；此前每来一段就把累积串整体重解析
（90 KB 的 `write` 峰值 ~140 MB 堆，500 KB 要几分钟）。

**prux 现状**：完全同构——`src/core/provider/anthropic.rs:1285`、`responses.rs:1009`、
`:1142`、`:1167` 都是 `slot.arguments = parse_streaming_json(&slot.partial_json)`，
而 `src/core/provider/json_args.rs:179` 的 `parse_streaming_json` 每次从头扫、必要时还做
全串 `repair_json`（`Vec<char>` 全量拷贝）。超大 `write`/`edit` 调用的 CPU 与内存增长曲线
与 pi 修前一致。

**建议**：移植一个增量解析器（栈式状态机 + 只对新增片段做 repair），`RawStreamEvent` 一侧
保留 `JsonChange` 式的增量信息。这是本区间**工作量最大**的一条，建议单独排期；短期缓解是
给 `parse_streaming_json` 加「长度超过 N 时降级为不产出部分参数」的闸门，避免用户侧卡死。

#### 1.16 ⚠️ `provider_session_headers` / 端点分流与上游不一致

**pi 1.5.0 Fixed**（#11434）：OpenCode Zen 与 OpenCode Go 各自的 OpenAI / Anthropic / Google /
System One 端点被混用；prux `src/core/provider.rs:697-712` 用
`provider/baseUrl 含 opencode.ai` 一个布尔判定注入 `x-opencode-session` / `x-opencode-client`，
`base_url` 由目录给。**需实测**：拿 `opencode`/`opencode-go` 各一条真实请求，确认协议端点
与两个头都在正确位置。

#### 1.17 ⚠️ Kimi K3.1 系列仍在使用 K2 的请求参数

**pi 1.5.0 Fixed**：K3.1 Pro / Highspeed 改发 `reasoning_effort`（low/high/max）、
回传早期 reasoning、接受中途新增工具，计费按 K3 家族。

**prux 现状**：请求参数来自目录 `thinkingFormat` / `sampling` 等字段，模型条目已随
`make sync-models` 更新。**需实测**一次 K3.1 的 thinking 与计费，确认没有残留 K2 假设
（`kimi-coding` 的 `k3-256k` 计费同样要核）。

#### 1.18 ⚠️ OpenCode DeepSeek 长缓存保留时发 `prompt_cache_retention` 被拒

**pi 1.5.0 Fixed**：OpenCode 网关不接受该参数。

**prux 现状**：是否发该参数取决于目录 `promptCache` 与 `cacheRetention` 路径。**需核对**
`src/core/provider/completions.rs` 的缓存保留参数拼装处，对 `opencode*` 直接不发。

#### 1.19 ⚠️ GPT-5.4/5.5 从别的模型切过来后报 `Missing namespace for function_call`

**pi 1.5.0 Fixed**（#11415）：这两个模型改在顶层 `tools` 里声明，不再走 `additional_tools`
/ tool search。

**prux 现状**：延迟工具（deferred）由 `tool_search` 激活后才进声明面
（`src/core/agent_session.rs:158`、`:9782`），是否给这两个模型下 `additional_tools` 需核对
`responses.rs` 的工具声明分支。**需实测**：先让 gpt-5.5 调一次工具，再切到 gpt-5.4/5.5 继续。

#### 1.20 ⚠️ provider 连接错误把底层原因藏起来

**pi 1.5.1 Fixed**（#11695）：connection refused / 证书错误不再被通用文案盖住。

**prux 现状**：`src/core/provider.rs` 的错误拼装已带 `display_full`（1.4.0 那条的产物），
但**需实测**一次 `https` 证书错误与一次 `127.0.0.1:1` 拒连，确认原因出现在给模型/用户的
文案里，而不只在日志里。

### C. 会话 / CLI / 信任

#### 1.21 🔧 没有「需要信任的资源」时，`trust.json` 里显式「不信任」被自动信任覆盖

**pi 1.5.0 Fixed**（#10947）：只有「没有任何已保存决策」时才自动信任。

**prux 现状**：三处都在 `has_trust_requiring_project_resources()` 为 false 时短路成
`true`，完全不看 `trust.json`：

- `src/cli/project_trust.rs:17`（启动期 `resolve_trusted`，**影响最大**：会话信任被设成 true）；
- `src/modes/gui/app/ui.rs:363`；
- `src/modes/tui/handlers/trust.rs:100`。

对「用户明确 distrust 过、项目里只有 `AGENTS.md`」的场景，`isProjectTrusted()` 会返回真，
MCP / 子代理 / agent-memory 全部按已信任处理。

**建议**：短路前先查 `find_nearest_trust_entry`，有决策就用决策。

#### 1.22 🔧 提示词模板参数解析：词内撇号吞词、无转义

**pi 1.5.1 Fixed**（#11861）：词内撇号（`doesn't`）不再开引号；无闭合引号按文本保留；
`\"` / `\'` 产生字面引号。

**prux 现状**：`src/core/prompt_templates.rs:146-172` 的单/双引号状态机没有「词内撇号」
判定也没有反斜杠转义，于是 `/review doesn't work` 会被吞成一个参数（`'` 之后直到下一个 `'`）。

**建议**：撇号只有出现在**参数起始位**时才算引号；补 `\` 转义；补三条用例
（词内撇号 / 未闭合 / 转义引号）。

#### 1.23 🔧 `/model` 与 `/resume` 的模糊搜索把「散落命中」排在「连续命中」之前

**pi 1.5.1 Fixed**（#11220 / #11296）：`fuzzyMatch()` 也要对每个连续出现位置打分取最优；
`/resume` 的 id 与消息文本改为精确子串匹配（`1024` 不再命中 UUID 里的片段）。

**prux 现状**：`src/utils/fuzzy.rs:11-63` 的 `fuzzy_match_score` 是**单次贪心扫描**
（第一个命中就吃），所以 `opus` 会把 `gpt-oss-120b` 这类散落匹配排在 `claude-opus-4.1` 前面；
同样逻辑另有份在 `src/modes/tui/session_selector.rs:1243` 与 `src/core/model_scope.rs:308`。

**建议**：`fuzzy_match_score` 改成「对 text 的每个起始位置匹配一次取最优」（O(n·m) 可接受，
候选集本来就是几十条），并删掉另两处复制实现、统一走 `utils::fuzzy`。

#### 1.24 🔧 `tool_search` 的分词只保留 ASCII 字母数字

**pi 1.5.0 Fixed**（#10816 / #11037）：中文/日文/泰文查询搜不到工具；现在按 Unicode
分词、无空格文本（`搜索记录`）也切词，且 `café` 与 `cafe\u0301` 视为等价。

**prux 现状**：`src/extensions/tool_search.rs:365-393` `tokenize` 末尾
`split(|c: char| !c.is_ascii_alphanumeric())` —— 任何非 ASCII 查询分词后为空，
于是「用中文找工具」永远零结果。prux 文档与界面的主要语言是中文，这条体感很差。

**建议**：改成「ASCII 按标点切 + CJK 逐字/双字切 + Unicode 归一化（NFD 折叠重音）」，
排序侧同时按 pi #11331 的口径加「名字命中权重高于描述提及」（见 2.10）。

#### 1.25 ⚠️ `/tree` 分支摘要不吃会话的 thinking 级别

> **第二轮结论：不改**。`simple_completion` 不发 effort 字段，被限额的账号不会撞 pi 那个 400；
> 补上 thinking 只会让摘要更贵。

**pi 1.5.0 Fixed**（#11444）：摘要请求会带上会话 thinking 级别，避免 reasoning 模型收到
不支持的 `none` effort 而拒绝。

**prux 现状**：`src/core/agent_session.rs:1899` 走 `core::provider::simple_completion`
只给 `max_tokens`，不带 thinking。**需核对** `simple_completion` 是否隐式加了 thinking 参数；
若没有，则 prux 反而不会撞这个 400，但摘要质量与一致性弱于 pi。

#### 1.26 ⚠️ 会话写入失败时，内存里留下了磁盘上没有的 entry

> **第二轮：已修**。`persist_mutation` 返回是否可应用，4 处调用改为「写成功才 `apply_mutation`」。

**pi 1.5.0 Fixed**（#11460）：写失败后后续 entry 指向不存在的父节点，恢复会话会丢历史与工具。

**prux 现状**：`src/core/session_v4.rs` / `session_manager.rs` 的追加路径**需核对**：
写盘失败后是否回滚内存中的 entry 与 leaf 指针。建议补一条「写失败 → 恢复会话」的用例。

#### 1.27 🔧 HTML 导出默认不带系统提示词与工具

**pi 1.5.0 Fixed**（#11261 / #11290）：导出（含 `pi --export`）现在包含 system prompt 与 tools。

**prux 现状**：能力已经有了（`src/core/export_html/export.rs:36` `export_session_extra`，
接收 `systemPrompt`/`tools`），`src/modes/tui/worker.rs:646` 也用了它；
但 `src/modes/tui/worker.rs:798`（另一条导出路径）、`src/modes/gui/bridge/worker.rs:289`、
`src/cli/startup.rs:188`（`--export`）都还在调不带 extra 的 `export_session`。

**建议**：把三处切到 `export_session_extra` 并传当前会话的 system prompt 与工具声明。
顺带确认 `assets/export-html/template.js` 已能渲染这两块（模板里的 `systemPrompt`/`tools`
分支若缺失要一起补）。

#### 1.28 ⚠️ steering / follow-up 消息里的图片没走缩放

> **第二轮复核：已满足**。busy/steer 提交走同一段 `expand_at_image_refs`（带 `current_image_resize_limits`），
> 图片消息经 `worker.prompt_message` 入队。

**pi 1.5.1 Fixed**（#11738）：队列里的图片按空闲输入的规格缩放，不按原图发送/保存。

**prux 现状**：图片缩放入口在 `src/utils/image.rs`，**需核对** `/steer` 与排队 follow-up
的图片是否也过这条路径（`src/modes/tui/handlers.rs` 的忙碌期提交分支）。

#### 1.29 ⚠️ `--model <provider>/<id>` 对含斜杠的 provider id 失败

> **第二轮：已修**。`split_provider_prefix` 从右往左按最长已知 provider id 切，都不已知时退回第一个 `/`。

**pi 1.5.0 Fixed**（#11394）：llama.cpp 从 URL 派生的 provider id 带斜杠。

**prux 现状**：prux 的 `--model` 解析在 `src/core/model_resolver.rs`，**需核对**是否按
「最后一个 `/` 之前全是 provider」切分。prux 的 `SUPPORTED_PROVIDERS` 里暂时没有带斜杠的
provider（`models.json` 自定义 provider 可以有），优先级低。

### D. TUI / 渲染

#### 1.30 🔧 Markdown 把 HTML 注释当可见文本渲染

**pi 1.5.1 Fixed**（#11710）：`<!-- ... -->` 不再显示为文字。

**prux 现状**：`src/modes/tui/render/markdown.rs:696` `Event::Html(t) => pending.push_str(t)`
——pulldown-cmark 把 HTML 注释也走 `Event::Html`，于是注释原样进画面。

**建议**：`Event::Html`/`InlineHtml` 里若是完整注释则丢弃（或统一按「块级 HTML 直接跳过」
处理），GUI 侧 `src/modes/gui/markdown.rs` 同步。

#### 1.31 ⚠️ 深度嵌套 blockquote 行数爆炸

> **第二轮**：只做了总行数闸门（`MAX_RENDERED_LINES` = 20000，超出截断并提示），引用深度计数未加。

**pi 1.5.0 Fixed**（#11272）：边框宽度耗尽后会指数级产出大量行，直到渲染卡死。

**prux 现状**：`src/modes/tui/render/markdown.rs:617-648` 递归渲染时
`width.saturating_sub(2).max(1)`——宽度到 1 就不再收缩，而每层都会把内层行**再按宽度 1 折一次**，
嵌套 20 层的一段长文本同样是乘法增长。**需实测**一段 30 层嵌套的引用是否卡住。

**建议**：加「引用嵌套深度上限」与「单条消息总行数上限」，超限直接降级为原文。

#### 1.32 ⚠️ 更宽的行内/块公式被逐行折行

**pi 1.5.0 Fixed**（#11483）：显示公式超宽时回退到原始 LaTeX，而不是把分式/求和拆散。

**prux 现状**：`markdown.rs:697-705` 的 `InlineMath`/`DisplayMath` 走自研 latex 渲染
（`src/core/latex.rs`）。超宽时的行为**需实测**：拿一条长分式在 40 列宽度下渲染。

#### 1.33 ⚠️ 样式词正好落在折行宽度时下一行多一个空格 / 删除线渗进 padding

**pi 1.5.0 Fixed**（#10925）。

**prux 现状**：折行在 `src/modes/tui/render/markdown.rs` 的 `wrap_line_spans`，是自研实现，
**需实测**（构造「加粗词正好填满一行」与「删除线跨行」两条用例）。这类 bug 单项影响小，
建议和其它折行修复合并处理。

#### 1.34 ⚠️ Markdown 每帧全量重渲染

> **第二轮：未做**。已有解析缓存（`parsed_events`），块级布局缓存需要性能基线与失效条件设计，本轮不动。

**pi 1.5.0/1.5.1 Fixed**（#10971）：复用上次渲染的顶层块，流式消息只重渲染最后一块；
`Text.setText()` 文本未变时不再重新折行。

**prux 现状**：`src/modes/tui/render/markdown.rs` 目前逐帧从事件流重建（`render_markdown`），
**无块级缓存**。长会话 + 流式输出时 CPU 明显偏高（prux 已在别处做过缓存/差分，这里没有）。

**建议**：按「顶层块」做 `Weak` 缓存，块文本 + 主题 + 宽度三者不变就复用；主题切换时整体失效。

#### 1.35 🔧 终端识别：Orca 缺位

**pi 1.5.0/1.5.1 Fixed**（#9329 / #11835）：Orca 被识别为支持 truecolor / 超链接，
并在声明 `ORCA_IMAGE_PROTOCOL=kitty` 时用 Kitty 图片。

**prux 现状**：`src/utils/terminal_caps.rs:150-190` 的白名单里没有 Orca；
`src/utils/terminal_image.rs` 的 `env_protocol()` 同样没有。Orca 用户会落到半块字符图片 +
`text (url)`。（1.4.0 那轮的 Rex 已经补上了，`terminal_caps.rs:184` 有注释。）

**建议**：两处各加一条 `TERM_PROGRAM` 判定；纯环境变量判定，成本极低。

#### 1.36 ⚠️ Kitty 协议的 F1–F12 与 F13+ 功能键

**pi 1.5.0 Fixed**（#11419 / #11345）：F1–F12（含带修饰）此前不识别，F13+ 会往编辑器里
插入不可见的私用区字符。

**prux 现状**：prux 已启用 kitty 键盘协议（`src/modes/tui.rs:527`，`REPORT_ALL_KEYS_AS_ESCAPE_CODES`），
`src/core/keybindings.rs:58` 支持 F1~F24。**需实测**：在 kitty/ghostty 里按 F5、Shift+F5、F13，
确认没有私用区字符进编辑器。

#### 1.37 ⚠️ 退格删 paste marker 会破坏后面的短 marker

**pi 1.5.1 Fixed**（#11923）：`[paste #2]` 被改坏后，提交的是 marker 文本而不是粘贴内容。

**prux 现状**：编辑器把大粘贴压成 `[paste #N +M lines]`（`src/modes/tui/handlers.rs:362`、
`:1288`），提交时展开（`:2537` 有用例）。**需核对** `editor.rs` 的删除路径是否会破坏
marker 文本后仍按 marker 展开（即「展开失败静默当文本发出」）。

#### 1.38 ⚠️ `pageUp`/`pageDown` 被列表与自动补全忽略

> **第二轮复核**：设置主列表、会话/面板选择器、`@` 补全本来就响应翻页；只缺设置**子面板**，已补。

**pi 1.5.0 Fixed**（#7629）：`SelectList` / `SettingsList` / 自动补全支持翻页键；
全屏模式下翻页键先给聚焦的组件，而不是滚动对话流。

**prux 现状**：`src/cli/session_picker.rs:342` 有 `KeyCode::PageUp`，但
`src/modes/tui/handlers.rs:907` 的 `move_page_up` 是编辑器动作。settings/模型/会话选择器与
`@` 补全是否响应 PageUp/PageDown **需实测**。这一条同时是 2.8 的前置。

#### 1.39 ⚠️ 终端标题里的控制字符

**pi 1.5.0 Fixed**：`setTitle()` 会把标题里的控制字符直接写进终端（可注入转义序列）。

**prux 现状**：没找到 `set_title` 的调用点（**可能本来就不设标题**）。若确实设置了，
要先剥 C0/C1；`utils/terminal_caps.rs` 那一带适合放 `replace_control_characters` 的等价物。

### E. MCP

#### 1.40 🔧 OAuth 元数据里的授权端点没有限制为 http(s)

**pi 1.5.0 Fixed（安全）**：授权端点/设备验证 URI 只接受 `http:`/`https:`；此前恶意 MCP
服务器可以给 `file:` 或自定义协议（如 `search-ms:`），登录时被平台 URL handler 拉起。

**prux 现状**：`src/extensions/mcp/oauth.rs:782-816` `fetch_authorization_metadata` 只解析
JSON，**不校验** `authorization_endpoint` / `token_endpoint` 的 scheme；
`oauth.rs:599` 的 scheme 检查只服务 `derive_application_type`。
`src/extensions/mcp/config.rs:462`/`:514` 校验的是**用户配置**里的回调/元数据 URL。

**建议**：元数据解析后逐字段校验 scheme；打开浏览器前再校验一次（`webbrowser` 调用点）。

#### 1.41 ⚠️ MCP 消息体大小上限 / 错误体截断 / SSE 连接释放

**pi 1.5.0/1.5.1 Fixed**（#11335 / #11729 / #11764 / #11761 / #11721）：JSON 响应绕过
`maxMessageBytes`、错误体整段读入、CR-only 行尾的 SSE 报「stream ended without a response」、
大 stdio 消息重复拷贝、stdio 子进程在 POSIX 上残留。

**prux 现状**：MCP 传输完全交给 `rmcp`（`src/extensions/mcp/manager.rs:30`
`streamable_http_client`），prux 自己**没有** message size 配置项。所以这些修复「随 rmcp
版本走」：**需核对** `Cargo.toml` 里 `rmcp` 的版本是否已含上述修复，否则升级依赖即可，
不需要在 prux 侧写代码。

### F. ACP 模式（`prux acp`）

#### 1.42 ⚠️ ACP 的退出清理与信号处理

> **第二轮：已修**。新增 `SHUTDOWN` 闩锁 + SIGTERM/SIGINT 监听：首个 EOF/信号让当前回合跑完，第二个才 `exit`。

**pi 1.5.0 Fixed**（#11599）：RPC 模式在空闲时忽略扩展的 `ctx.shutdown()`；stdin EOF 与
SIGTERM 同时到达时（Paseo / systemd 就是这么干的）清理被截断，结果丢了刚刷新的 MCP token、
MCP server 反过来把登录吊销；现在「一旦开始关闭，EOF 与单个信号都不再打断，第二个信号才立即退出」。

**prux 现状**：`prux acp` 是 stdin/stdout 上的 NDJSON + JSON-RPC 服务端
（`src/extensions/acp/server.rs:49` `run()` 跑「直到 stdin 关闭」，`:212` 在 stdin 关闭时丢弃本轮
future 后退出）。**没有**信号处理与「关闭已开始」的闩锁：EOF 与 SIGTERM 同时到达时，
MCP token 刷新、会话落盘这类收尾工作有被切断的风险（退出码直接返回，无等待）。

**建议**：加一个「shutdown 已开始」的标志：首个 EOF/信号只置标志并等待在途清理完成，
第二个信号立即 `exit`。顺带确认空闲时扩展请求的退出（`ctx.shutdown()` 等价物）能立即生效。

---

## 二、本区间新增 / 变更功能（候选移植）

### 2.1 🆕 Anthropic message threads（服务端续接）

**pi 1.5.1 Added**：带 `sessionId`（且 `cacheRetention != "none"`）的请求改用
`message-threads-2026-08-12` beta，只发新消息、由服务端续接上次回复；上下文有任何变化
或服务端续不上时回退整包请求。默认只在 `api.anthropic.com` 开启，
`compat.supportsMessageThreads` 可改；会话结束调 `cleanupSessionResources(sessionId)`。
另加 `StreamOptions.disableContinuation`（缓存保温这类「重放同一请求」的场景要关掉）。

**prux 现状**：无。`src/core/provider/anthropic.rs` 的请求体没有 `thread` 字段；
`src/core/cache_warmer.rs` 的重放请求与它天然冲突（正好是 `disableContinuation` 的场景）。

**建议**：先实现「默认关闭 + `compat.supportsMessageThreads` 开启」，把 cache warmer 的重放
固定为不带 thread；不实现 `cleanupSessionResources` 的话，长会话的服务端状态会攒着——
可以按「会话结束/切换时补一次清理请求」处理。收益是长会话省下大量 prompt token，
但**风险**在「服务端续接与本地上下文不一致」，必须有可靠的整包回退。

### 2.2 🆕 空闲缓存保温成为默认，并合并计费行

**pi 1.5.0/1.5.1 Changed**：`cacheWarming` 默认从 `streaming` 改为 `idle`（跑完还能保 30 分钟），
且连续的保温合并成一行 `Cache warmed: $<总额>`。

**prux 现状**：`idle` 模式**已实现**（`src/core/cache_warmer.rs:26-33`，30 分钟上限、
`IDLE_CONTINUATION_PROBABILITY = 0.15` 的收益折算、$0.05 门限都在），但
`settings.json` 缺省值是 **`off`**（`src/core/settings_manager.rs:1319`）。

**建议**：这是**默认值决策**，不是缺功能：默认 `idle` 会替用户花钱（虽有小额门限）。
建议保持 `off`，但在 `/settings` 的说明里点明 `idle` 的存在；若要与 pi 对齐再改默认值，
并在 changelog 里明写「默认会保温」。合并计费行属可选打磨。

### 2.3 🆕 `prux auth login/logout`（不起会话的凭据子命令）

**pi 1.5.1 Added**：`pi auth login <provider>`（浏览器登录、`--api-key`、`--no-browser` 设备码）
与 `pi auth logout <provider>`。

**prux 现状**：`src/cli/` 已有 `auth` 子命令（见 `cli/auth.rs` 一族的实现，
`src/core/login_registry.rs` 提供 provider 列表）。**需核对**子命令形状是否已覆盖
「不起会话直接登录 / 登出 / 存 API key / 无浏览器」四种，缺哪种补哪种。

### 2.4 🆕 自定义自更新命令

**pi 1.5.0 Added**：`PI_SELF_UPDATE_COMMAND` 环境变量与全局 `selfUpdateCommand` 设置，
`pi update` 时用它（`{packageName}` / `{version}` / `{installSpec}` 占位）。

**prux 现状**：`src/extensions/update_check.rs` 是「检查 + 提示 + 打开页面」，没有自更新执行。
**建议**：仅当 prux 真的要支持自更新时再做；否则明确不做（用户走包管理器）。

### 2.5 🆕 扩展包更新后的 changelog 提示

**pi 1.5.0 Added**：`package.json` 的 `pi.changelogPath` 或包根 `CHANGELOG.md`，
下次启动在 "Extension Updates" 里显示比上次看到更新的条目；`collapseChangelog` 压成一行。

**prux 现状**：prux 的扩展是**编译期内置**（`linkme` 切片），没有「扩展包」概念，
`/changelog` 只内嵌当前版本（`src/core/changelog.rs`）。❌ 结构上不需要。

### 2.6 🆕 Mermaid 支持的图类型变多

**pi 1.5.1 Changed**：新增饼图、作者自定义样式、支持超链接终端的可点击标签，且分支/压缩
摘要里也能渲染。

**prux 现状**：`src/vendor/mermaid_text` 是上游 `mermaid-text` 0.57.0 的副本，
支持哪些图类型由该版本决定。**建议**：查上游新版本是否已带饼图/样式；有就同步 vendor
（记得同步时重跑 `crate::` 路径改写 + `cargo fmt`，见 AGENTS.md），没有就记为不实现。

### 2.7 🆕 语法高亮补 Dart / Nix / Groovy / Gradle

**pi 1.5.1 Added**（#11643）。

**prux 现状**：`src/modes/tui/render/syntax_hl.rs:33-48` 用 syntect 默认集
（`load_defaults_nonewlines`）**外加**一张 `assets/syntaxes/Zig.sublime-syntax`。
**需核对**这四种语言在 syntect 默认集里的覆盖情况，缺的按 Zig 的方式加进 `assets/syntaxes/`
（注意 `embedded!` 是编译期内嵌，新增文件要同步加载清单与 `src/core/doc_sync` 之外的内嵌表）。
GUI 侧 `src/modes/gui/code.rs:16` 只用了默认集，需要共用同一 `SyntaxSet`。

### 2.8 🆕 `compat` 新字段（协议兼容开关）

**pi 1.5.0/1.5.1 Added**：

| 字段 | 作用 |
|---|---|
| `requiresNonNullAssistantContent` | 只有工具调用时 `content` 发 `""` 而非 `null`（见 1.12） |
| `requiresToolsWithToolHistory` | 历史里有工具调用时仍发 `tools: []`（老 LiteLLM 需要） |
| `supportsMessageThreads` | 开关 2.1 |
| `promptCacheKeyMode`（`auto`/`enabled`/`disabled`） | 开关 prompt cache key（prux **根本不发** `prompt_cache_key`，此字段暂无意义） |
| `refreshModelsAuth: "configured"` | 目录刷新不需要凭据的 provider 跳过 token 刷新（prux 的 `model_refresh` 可参考） |

**prux 现状**：`compat` 由 `src/core/model_resolver.rs:216`/`:279` 合并，字段名是驼峰、
和 pi 一致，加字段基本是「读字段 + 用字段」两处。

### 2.9 🆕 分类器：GitHub Copilot 的 Microsoft-Decision-1

**pi 1.5.1 Added**（#11679）：经 Copilot 调 Microsoft-Decision-1（含 codemode `models.classify()`）。

**prux 现状**：`src/core/provider/classifier.rs` 已有 System One 系实现
（`typesafe-system-one`）与 OpenRouter 决策模型，Copilot 通道没有。
目录条目已随 `make sync-models` 进来（`github-copilot` 下的 classifier 条目），
**建议**：先看目录里有没有该条目，有就补一条协议实现（请求形状参考现有 System One）。

### 2.10 🆕 `tool_search` 排序口径统一

**pi 1.5.0/1.5.1 Fixed**（#11331 / #11340 / #11625）：查询词命中「名字」权重大于描述提及、
整名命中给大额奖励、server instructions 只作低权字段；另加 `$ref` 后的属性名/描述也能被搜到。

**prux 现状**：`src/extensions/tool_search.rs` 有自己的 BM25 式打分与 `MAX_REF_EXPANSIONS`
（`declarations.rs:8`），`$ref` 展开**已有**。排序权重是否与 pi 对齐**需实测**
（`thread_read` vs `queue_read` 类混淆）。建议与 1.24 的分词一起改，改完补排序用例。

### 2.11 🆕 计费：`totalFromProvider` 与 OpenRouter 上报金额

**pi 1.5.0/1.5.1 Added/Fixed**（#11178 / #9980 / #11948）：`Usage.cost.totalFromProvider`
区分「供应商上报的账单」与「目录估算」；OpenRouter 在 Completions / Anthropic Messages
上的总额取上报值（BYOK 计入上游费用），Azure 保留 OpenAI 的长上下文档位。

**prux 现状**：`src/core/provider/usage.rs:34-76` `compute_cost` 一律按目录价算，
`Cost`（`provider.rs:1034`）没有「上报」标记；`service_tier_cost_multiplier` 已有配角。
Azure 档位是数据面（已 sync）。

**建议**：`Cost` 加 `total_from_provider: bool`，在 OpenRouter 解析响应的 `usage.cost`
时置真并覆盖 `total`（`x-stats`/`usage` 字段口径按 pi 的 `openrouter` 实现核对），
展示侧（footer / `/cost`）标注「按供应商上报」。中等收益，改动集中在 usage 一处。

### 2.12 🆕 `qwen-token-plan-individual-cn` provider

**pi 1.5.1 Added**（#7989）：Qwen Token Plan Individual 的中国端点，凭据
`QWEN_TOKEN_PLAN_CN_API_KEY`。

**prux 现状**：`SUPPORTED_PROVIDERS`（`src/core/model_resolver.rs:31-67`）有
`qwen-token-plan-individual`，**没有** `-cn` 变体；`default_model_id` 同样缺一条。
**建议**：加 provider 字符串 + `QWEN_TOKEN_PLAN_CN_API_KEY` 的环境变量映射
（`src/core/auth.rs`），默认模型 `qwen3.8-max` 与 pi 对齐。

### 2.13 🆕 会话工作目录消失时提供 Fork（1.5.2）

**pi 1.5.2 Added**：原目录被移动/删除时，交互模式给 `Continue` / `Fork` / `Cancel` 三选；
`Fork` 把会话复制到当前目录成为新会话（新 id），下次 resume 不再追问。适用于启动
（`--session`/`--resume`）、`/resume`、`/import` 与扩展的 `switchSession()`。

**prux 现状**：`--fork` 存在（`src/core/session_manager.rs`），但**没有**「目录不存在 → 三选」
的交互；`src/cli/cwd_store.rs` 那一族是候选落点。相关修复：pi 同一 PR 修了
`/import` 目录不存在时留下重复副本（`s.jsonl` + `s-1.jsonl`）。

**建议**：先做「Ctrl 提示 + Fork 选项」，`/import` 的副本残留一并修（用同一段复制逻辑，
失败时删掉半成品文件）。

### 2.14 🆕 扩展 UI 的 `blocked` / 组件焦点 / 元数据

**pi 1.5.0/1.5.1 Added**：`ctx.ui.custom()` 的 `blocked` 选项（覆盖层显示期间上报
OSC 7501「等我」）；`Component.capturesViewportInput`（聚焦组件吃掉 PageUp/PageDown）；
`MarkdownTransformContext` 带持久化的 `messageId` 与 ISO `timestamp`；
`keyText()`/`keyHint()` 注入到渲染上下文。

**prux 现状**：`src/modes/tui/program_status.rs` 已有 OSC 7501 全链路；覆盖层是单槽位
（`app.overlay: Option<OverlayState>`），没有 `blocked` 概念；扩展渲染走 Rust trait，
没有「注入函数」的需求。**建议**：只做「覆盖层上报 blocked 状态」这一条（1 小时量级），
其余不适用。

### 2.15 数据面已同步（模型条目）

`assets/models/**` 已随上游 1.5.2 重新生成（含 `claude-fable-5-2`、`gpt-6.1-terra`、
`kimi-k2.7-code`、`MiniMax-M3`、`gemini-4-argon`、`glm-5.3`、`microsoft-decision-1` 等）。
**不要手改**这些文件。

---

## 三、移除 / 破坏性变更（对齐时容易踩的坑）

| # | 变更 | prux 影响 |
|---|---|---|
| 3.1 | `packages/ai` / `agent` / `durable` / `schema`：工具参数改由 `@earendil-works/pi-schema` 校验与强制转换，TypeBox 特性（`Type.Refine`/`BigInt`/`Undefined`/codec/`Format.Set`）不再生效 | ❌ 不适用：prux 的工具参数校验是 Rust 侧手写（`core/tools/index.rs` 的校验 + `json_args` 的修复），没有 TypeBox 概念 |
| 3.2 | OpenAI 兼容端点不再发 `"tools": []`（#11843） | ✅ 已满足：`src/core/provider/completions.rs:821-831` 在 `tools.is_empty()` 时就不发该字段。`requiresToolsWithToolHistory` 作为可选开关见 2.8 |
| 3.3 | `StreamingJsonParser` 重写、`appendGrammarToolInputJsonDelta()` 签名变更、grammar 工具输入增量 | ⚠️ 见 1.15（性能面等价项） |
| 3.4 | 移除原生 OS `sandbox/` 扩展示例及相关依赖 | ❌ 不适用：prux 无该示例（`src/core/sandbox.rs` 是权限判定，不是 OS 沙箱示例） |
| 3.5 | `packages/durable`：`watchEvents()` / `Storage.commit()` / 全部 watch 形状重做；`packages/chord`：状态 getter 移除 | ❌ 不适用：prux 无 durable/chord 层（`extensions/tasks` 自研任务图，语义不同） |
| 3.6 | `packages/env`：`RemoteWatcher.close()` → `stop()`、变更 tagged；daemon 新增 `process` 操作 | ❌ 不适用：prux 的 `extensions/ssh_remote` 是另一套实现 |
| 3.7 | `packages/mcp`：OAuth 登录拆成两半（`startMcpSignIn`/`completeMcpSignIn`）、新增 `PipeTransport`、工具命名/输出截断助手搬到包内 | ❌ 不适用：prux 用 `rmcp` 的 `AuthorizationManager`，这些 API 是给外部宿主用的 |
| 3.8 | `packages/telemetry` / `schema` / `codemode`：新增 `telemetryNow()`、`resolveSchemaRef()`、`unsafe<T>()`/`recursive<T>()` 等 | ❌ 不适用（prux 无 telemetry 层；codemode 的自研 `$ref` 展开已覆盖，见 2.10） |
| 3.9 | `pi update` 不再给 `npm ci` 传 `--min-release-age=0`；`-e npm:<pkg>` 每次启动重装最新匹配版本 | ❌ 不适用：prux 没有 npm 包安装 / 自更新链路 |
| 3.10 | 默认 OpenAI 模型 `gpt-5.5` → `gpt-6.1-sol`；`anthropic` → `claude-opus-5-5` | 🔧 见第四节（prux 默认表要改） |

---

## 四、模型目录 / 默认模型表（第二轮已完成）

`default_model_id` 已按 pi 的 `defaultModelPerProvider`（`packages/coding-agent/src/core/model-resolver.ts:20`）
逐项跟到 1.5.2；`qwen-token-plan-individual-cn` 已作为独立 provider 加入，
模型数据由 `scripts/sync-models.py` 的 `DERIVED_PROVIDERS` 规则派生（中国目录 ∩ Individual 白名单），
`make sync-models-check` 幂等。下表保留为「实施前」对照：

| provider | prux 现值 | pi 1.5.2 |
|---|---|---|
| `anthropic` | `claude-opus-4-8` | `claude-opus-5-5` |
| `openai` | `gpt-5.5` | `gpt-6.1-sol` |
| `google` | `gemini-3.1-pro-preview` | `gemini-4-argon` |
| `moonshotai` / `moonshotai-cn` | `kimi-k2.6` | `kimi-k2.7-code` |
| `huggingface` | `moonshotai/Kimi-K2.6` | `moonshotai/Kimi-K2.7-Code` |
| `opencode` | `kimi-k2.6` | `kimi-k2.7-code` |
| `openrouter` | `moonshotai/kimi-k2.6` | `moonshotai/kimi-k2.7-code` |
| `minimax` / `minimax-cn` | `MiniMax-M2.7` | `MiniMax-M3` |
| `baseten` | `zai-org/GLM-5.2` | `zai-org/GLM-5.3` |
| `vercel-ai-gateway` | `zai/glm-5.1` | `zai/glm-5.3` |
| `qwen-token-plan` / `-cn` | `qwen3.7-max` | `qwen3.8-max` |
| `opencode-go` | `kimi-k3` | `kimi-k3.1-pro` |

已一致、无需动：`azure`、`github-copilot`（都是 `gpt-6.1-sol`）、`nvidia`、`xai`、`groq`、
`cerebras`、`zai`、`zai-coding-cn`、`kimi-coding`、`meta`、`fireworks`、`together`、`deepseek`、
`ant-ling`、`xiaomi*`。

`qwen-token-plan-individual-cn` 已补（见 2.12），`auth.rs` 里走 `QWEN_TOKEN_PLAN_CN_API_KEY`。

> 改完这套表，`model_resolver.rs` 里那条「`openai` 目录应保留 `gpt-5.4`」之类的断言要一并核对，
> 别把 `openai` 与 `github-copilot` 混改。

---

## 五、已满足 / 结构性不适用（汇总，不逐条开工）

| pi 条目 | prux 结论 |
|---|---|
| grep 受 `RIPGREP_CONFIG_PATH` 影响（1.4.0 遗留） | ✅ `grep.rs:83` 已有 `--no-config` |
| `find`/`ls` 的 `limit < 1` | ✅ 两处都 `.max(1)`（`find.rs:46`、`ls.rs:41`） |
| HTML 导出因数据内联方式白屏（#11225） | ✅ 数据走 base64 内嵌（`export.rs:46`、`:58`），`</script>` 之类不会截断模板 |
| HTML 导出 CJK 强调（1.4.0 遗留） | ✅ `assets/export-html/vendor/marked-cjk-friendly.min.js` 已内嵌 |
| 裸 Markdown URL 吞 CJK 标点（#10934） | ✅ 不适用：prux 未开 pulldown-cmark 的 GFM 裸链接自动识别，裸 URL 不会变成链接 |
| Alacritty 无 `TERM_PROGRAM` 时超链接失效（#11767） | ✅ `terminal_caps.rs:184` 已按 `TERM` 含 `alacritty` 判 truecolor，超链接默认开启 |
| Rex 终端（1.4.0 遗留） | ✅ `terminal_caps.rs:190` 已认 `TERM_PROGRAM=rex` |
| 终端光标哨兵泄漏（#9441） | ✅ 不适用：prux 不用「光标哨兵」文本标记（编辑区走独立渲染） |
| 全屏双击选择在 `~ . \ +` 处断词（#10980） | ⚠️ 需实测 `src/modes/tui` 的词边界表 |
| 窗口标题控制字符 | ⚠️ 见 1.39（prux 可能不设标题） |
| `~2/project` 之类的错误 `~` 前缀 | ✅ 不适用：`footer.rs:99-114` 用 `Path::strip_prefix` 按组件比较，`/home/me2` 不会被当成 `/home/me` |
| Windows 路径大小写不敏感比较（#10488） | ⚠️ 需核对 `utils/paths.rs` 的归一化是否在 Windows 上小写化（技能去重、trust 键、`--continue` 都用它） |
| `pi.exec()` 的 `maxBuffer` / UTF-8 跨 chunk / spawn 失败 stderr（#11360/#11312/#11257） | ❌ 不适用：prux 扩展是 Rust trait，无 `pi.exec()` |
| RPC 模式的 shutdown / signal 处理（#11599） | ⚠️ 见 1.42（`prux acp` 就是 stdio JSON-RPC 服务端） |
| `pi update --extensions` 的逐包版本变化、`git clean -fdx` Windows 失败（#11707/#11597） | ❌ 不适用：无 npm / git 包安装 |
| OAuth 刷新在退出时丢失新 token（#11692） | ⚠️ 需核对 `core/auth.rs` 的刷新锁与「退出等待保存」 |
| `auth.json` 权限 0600、符号链接下的锁共享（#11655） | ⚠️ 需核对 `core/auth.rs`/`utils/file_lock.rs` 是否按**解析后**路径加锁 |
| `pi config` 忽略 `keybindings.json`（#11651） | ⚠️ 需核对 prux 的 `/settings` 与 `keybindings.json` 的合并路径 |
| Ctrl+V 在 Windows/WSL 粘贴（#11765） | ⚠️ 需实测（`src/modes/tui/handlers.rs` 的粘贴键表） |
| 设备码登录按 Enter 开浏览器 + 复制码（#9282） | ⚠️ 需核对 `src/core/oauth.rs` / `cli/oauth_flow.rs` 是否已有 |
| `/login` 列表把 Radius/OpenRouter 置顶（1.5.1） | ❌ 不适用：prux 无 Radius 合作身份 |
| 模型选择器/搜索被后台刷新挪动选中项（#9109） | ⚠️ 需实测 `/model` 在目录刷新到达时的选中位置 |
| 技能警告标签 `[Skill conflicts]` → `[Skill warnings]`（#11521） | ⚠️ prux 自研文案，按中文提示统一即可，无需照搬 |
| 信任探针之外的 `pi install/remove` 路径问题（#11358） | ❌ 不适用 |
| 分类器 `compat`（`imageInput`/`sendImageOptInHeader`，1.4.0 遗留） | ⚠️ 仍未做，见 `migration-v1.4.0.md` 2.1/2.2；与本轮无冲突 |
| 图片缩放 PNG→更小编码（#11551） | ⚠️ 需核对 `utils/image.rs` 是否已按「取较小编码」决定 JPEG/PNG |
| 非 BMP 字符被截半（#7121） | ✅ Rust `truncate` 按字符边界切，不会留下孤立代理项 |

---

## 六、未收录 / 待人工比对

1. **「Tightened system prompt」（pi 1.5.0 Changed）**：上游只说「收紧」，没有可对齐的条目。
   prux 的系统提示词是自研（`src/core/system_prompt.rs`），**建议人工对比一次**上游
   `system-prompt` 相关文件，只挑「能明确写出差异」的段落跟进，不要逐字抄。
2. **`/model` 与 `/scoped-models` 在 id 旁显示名字（#5916）**：纯展示打磨，prux 的选择器
   已有 name 字段，按需再做。
3. **`pi update` 系列、`/login` 视觉、Mermaid 可点击标签、`select` 的 `✦` 标记**：属于 pi 独有
   的发行/合作面，不跟。
4. **`pi -e npm:` / git 扩展包的缓存语义（#11375）**：prux 无扩展包机制。
5. **`undici` 升级、第三方 license 目录改名**：不适用。
6. 各包 `[Unreleased]`（1.5.2 之后）不计入本轮。

> 落地时按 AGENTS.md 的提交纪律：一类改动一次提交、显式 `git add <文件>`，
> 并同步 `assets/changelog/v.1.5.2.md`（一个功能一条，中英两节逐条对应）。
