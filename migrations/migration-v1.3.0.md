# Prux 迁移指南：pi 1.1.0 → 1.3.0

> **来源**：pi 仓库 tag `v1.1.0` → `v1.3.0` 之间各包 `packages/*/CHANGELOG.md` 的
> `[1.2.0]`、`[1.3.0]` 两个 release 段
> （`git diff v1.1.0..v1.3.0 -- 'packages/*/CHANGELOG.md'`，共 144 个非 merge 提交），
> 并用 `git log/show v1.1.0..v1.3.0` 对关键条目逐条核对源码。
> prux 基线：pi **1.1.0**（上一份指南 `migrations/migration-v1.1.0.md`，已全部收口）。
> 本轮资产侧已由提交 `b392431` 预置：`Cargo.toml version = 1.3.0`、`sync.md = 1.3.0`、
> `assets/models/**` 已同步（新增 102 / 移除 40 个模型）、`assets/changelog/v.1.3.0.md`
> 与本文档为**空占位**，内容待补。
>
> **收录规则**（按用户要求）：只收录
> ①「prux 已实现功能的 bug 修复」与
> ②「1.1.0→1.3.0 区间内 pi 新增/变更/移除的功能」。
> prux 没有、且在本区间之前就已存在的 pi 功能（Bedrock / Mistral / Radius / llama.cpp /
> `openai-codex` / Node 打包与 Nix 打包面 / `packages/{durable,env,telemetry}` 的既有能力）
> **不逐条列出**，只在第五节汇总说明。

**状态标记**：🔧 应修/应移植　⚠️ 需对照 pi 逐项核对（口径可能已有偏差）　✅ 已满足/无此问题（附理由）
❌ 结构性不适用（prux 无对应实现，无可改）　🆕 本区间新增/变更的候选移植项　🚫 **决定不实现**（沿用或新增的终局决策）

---

## 零、总体判断

| 分类 | 条目 | 数量 |
|---|---|---|
| ✅ 已落地（bug 修复，2025 本轮全部完成） | 1.1–1.15，1.17，1.18 | 17 |
| ⚠️ 待定（prux 未实现 `strict`，无 bug 可修） | 1.5 | 1 |
| ⚠️ 可选（read 标签显示口径，与 pi 本就不同） | 1.16 | 1 |
| 🆕 候选移植（区间新增能力，prux 尚缺） | 2.1–2.8 | 8 |
| ✅ 已满足 / 数据面已同步 | 2.10–2.15，3.1–3.4、3.9–3.10 | 12 |
| 🚫 决定不实现 | 2.9（`supportsMidConvoEffort`）、3.7（`mcp login --timeout`，沿用 1.1.0 决定） | 2 |
| ❌ 结构性不适用 | 2.16–2.21，3.5、3.6、3.8、3.11–3.13，以及第五节整段 | — |

> 上表是**终局状态**：第一节的 bug 修复已全部落地（明细见第七节），剩下的 1.5 / 1.16
> 为可选或口径差异，第二～四节里的 🆕 项尚未开工。
> 验收：`cargo test` 全绿（lib 3602 + 全部集成测试目标）、`cargo fmt --check` 通过、
> `cargo clippy --lib --tests` 只剩改动前就存在的 3 条 warning（`settings_manager` 1、`tasks/widget` 2）。

**建议实施顺序**（按性价比）：

1. **P0 数据面/文案面收尾**：`assets/changelog/v.1.3.0.md` 补内容（1.3.0 的 changelog 段可直接
   改写自本文件第二、三节）；`login_registry.rs:125` 的 kimi-coding `base_url` 补 `/v1`（2.10 附注）。
2. **P1 重试与工具调用健壮性**：1.1（三条重试串）、1.2 + 1.3（非法 `\u`、畸形参数不静默执行）。
   这三条是用户可见的失败/静默错误，改动集中、测试便宜。
3. **P2 provider 正确性**：1.6（同 chunk 顺序）、1.7（Gemini 4 thinkingLevel）、1.8（切模型丢 thinking）、
   1.4（Anthropic error tool result 图片）。
4. **P3 会话/设置一致性**：1.9（空引号）、1.10（`~`）、1.11（恢复会话的 active tools）、1.12（摘要不含 thinking）、
   1.13（codemode `@options`）、1.14（键位 `+`）、1.15（`--model` 歧义）。
5. **P4 新能力（可延后）**：2.1–2.8。其中 2.1/2.2/2.3/2.4 面向真实用户，2.5–2.8 属 API 对齐。

---

## 一、Bug 修复（prux 已有功能，升级时应评估移植）

### 1.1 🔧 三条瞬时 provider 错误未命中重试表

**pi 1.2.0 / 1.3.0 Fixed**：`The upstream provider returned an error`（#10842）、
`The service is temporarily unavailable`（#10965，同时导致压缩首次尝试即失败）、
gateway `Unable to forward upstream stream`（#10920）现在都按瞬时错误重试。

**prux 现状**：`src/core/agent_session.rs:2331-2389` 的 `RETRYABLE` 里**都没有**这三条
（只有 `provider returned error`、`service unavailable`，都对不上：前者少了 `an`，后者少了前缀）。

**建议**：在「服务端瞬时负载」分组补
`"the upstream provider returned an error"`、`"the service is temporarily unavailable"`、
`"unable to forward upstream stream"`，并在 `retryable_error_classification_matches_pi` 补断言。

### 1.2 🔧 工具调用参数里的非法 `\u` 转义导致字段丢失

**pi 1.3.0 Fixed**（#10868）：参数串里出现 `\usepackage` 这类非法 `\u` 转义时，
解析会丢掉字段；pi 现在先修复该转义。

**prux 现状**：`src/core/provider/usage.rs:105-128` `parse_streaming_json` 在完整解析失败后
回落到 `partial_parse`（从后往前截断取最长可解析前缀），非法转义会让整串解析失败，
于是只留下截断出来的部分对象。`src/core/provider/convert.rs:33-36` 的 `sanitize_surrogates`
是 no-op（Rust 的 `String` 本就不含孤立 surrogate），与 pi 的修复目标不是一回事。

**建议**：在拼接完 `arguments` 原文、交给 `serde_json` 之前，把 `\u` 后不接 4 位十六进制的
序列改写成 `\\u`。该入口被 completions / responses / anthropic / google 共用，一处即可。

### 1.3 🔧 最终参数为畸形 JSON 时用「部分解析前缀」静默执行

**pi 1.3.0 Fixed**（#10897）：工具调用最终参数是畸形 JSON 时，此前会拿错误前解析出的字段
静默执行；现在调用带上 `argumentsError`，校验失败并把**原始参数字符串**回给模型
（ai 侧 `argumentsError` 字段，见 `packages/ai/CHANGELOG.md` [1.3.0]）。

**prux 现状**：`usage.rs:104-118` 的 `partial_parse` 回落正是旧行为；
`ContentBlock::ToolCall`（`src/core/provider.rs:497-515`）没有 `arguments_error` / 原始文本字段；
`src/core/tools/index.rs:183-243` 的 `validate_tool_arguments` 只报 schema 错误与「已解析参数」的
pretty 文本。

**建议**：解析失败时在 `ToolCall` 上记录 `arguments_error` 与原始参数文本，校验阶段直接判失败
并把原文回给模型，不再用部分字段执行。与 1.2 同属一条修复链，可一起做。

### 1.4 🔧 Anthropic：error tool result 携带图片导致 400

**pi 1.3.0 Fixed**（#10794）：error tool result 内含图片时 Anthropic / Bedrock Claude 请求 400；
pi 把图片移到该回合全部 tool result 之后另起一条消息。

**prux 现状**：`src/core/provider/anthropic.rs:394-432` `push_anthropic_tool_result` 把
`convert_content_blocks(&msg.content)`（含 image 块）原样内联进 `tool_result.content`，
`is_error` 只是字段。Bedrock 部分不适用（`SUPPORTED_PROVIDERS` 无 `amazon-bedrock`）。

**建议**：当 `msg.is_error` 且 content 含 image 时，把 image 块抽出，放到该回合全部
tool result 之后的独立 user 消息里，模型仍能看到图片。

### 1.5 ⚠️ Anthropic strict 工具集被 provider 拒绝后回落重试

**pi 1.2.0 Fixed**（#10409）：`strict: "prefer"` 的工具集因 grammar 体积 / 工具数 / union 上限被拒时，
pi 以非严格重试一次，`strict: "require"` 的工具保持严格。

**prux 现状**：`src/core/provider/anthropic.rs:573-603` `convert_anthropic_tools` 只发
`name` / `description` / `input_schema` 等，**根本不发 `strict`**（全仓无 anthropic 侧 strict 生成）。
因此 pi 的这个 bug 在 prux 上不会触发，但 prux 也享受不到严格工具调用的收益。

**建议**：不算 bug，属缺能力。若将来移植 `strict`，必须**同时**移植这次回落重试，否则会引入同一个 400。

### 1.6 🔧 OpenAI 兼容流同一 chunk 内 reasoning + content 导致 thinking 排在 text 之后

**pi 1.3.0 Fixed**（#10926）：LiteLLM 之类的代理把 reasoning 与 content 放在同一个 stream chunk 时，
回答后面会再冒出思考；pi 修正了排序。

**prux 现状**：`src/core/provider/completions.rs:1130-1135` `handle_delta` 先
`handle_content_delta` 再 `handle_reasoning_delta`，而内容块下标由 `next_content_index`
按**首次出现顺序**分配（`:1153`、`:1191`）。同 chunk 两者都有时，text 拿到更小的下标，
渲染出来就是「答案在前、思考在后」。

**建议**：同一 delta 内先处理 reasoning（或先为 thinking 分配 `thinking_index`），
保证 thinking 块永远排在 text 之前。

### 1.7 🔧 Gemini 4 未走 `thinkingLevel`（Vertex 面不适用）

**pi 1.3.0 Fixed**（#10890）：Gemini 4 Argon 在 Vertex 上因 `Thinking budget is not supported` 失败、
在 Gemini API 上 thinking level 被忽略；pi 对 Gemini 4 及以后统一发 `thinkingLevel`。

**prux 现状**：`src/core/provider/google.rs:68-73` `uses_google_thinking_level` 只认
`gemini-3[.x]-pro|flash`、`gemini-flash-latest`、`gemini-flash-lite-latest`、Gemma 4；
目录里已有 `gemini-4-argon`（`assets/models/models.all.json:5290`），会走 `thinkingBudget` 分支。
Google Vertex 不适用（无该 provider）。

**建议**：把 `gemini-4*`（及以后主版本）纳入 `uses_google_thinking_level`，并补该模型的用例。

### 1.8 🔧 切换模型后仍把其它模型产生的 thinking 发给新模型

**pi 1.3.0 Changed**（#10995）：切模型后，`serializeConversation` 之外的请求构造会**丢弃**历史里
由别的模型产生的 thinking 块，而不是当普通文本发出去（旧模型推理被新模型当成自己的话，会开始
把推理写进可见回答）。

**prux 现状**：`src/core/provider/anthropic.rs:290-296` 已经算出 `is_same_model`，
但 `:322-346` 的处理是：无 signature → 降级成 `type: "text"`（**不看** `is_same_model`）；
有 signature → 不论是否同模型都发 `type: "thinking"`（redacted 分支才校验同模型）。

**建议**：`!is_same_model` 时直接丢弃整个 thinking 块（redacted 已经这么做）。这条同时影响
Anthropic / OpenAI / Google 三套 convert 路径，需逐套核对。

### 1.9 🔧 prompt 模板丢弃空引号位置参数

**pi 1.3.0 Fixed**（#10867）：`""` / `''` 空参数此前被丢弃，导致后续位置参数整体前移、
`${N:-default}` 的默认值落不上。

**prux 现状**：`src/core/prompt_templates.rs:145-169` `parse_command_args` 用
`if !current.is_empty()` 决定是否 push，空引号解析出空 token 后永远不 push。

**建议**：加「token 是否已开始」标志（引号一开即置位），空引号也 push `""`；
补 `parse_command_args("a \"\" b") == ["a", "", "b"]` 用例。

### 1.10 🔧 skills 过滤条目里的 `~` 不展开

**pi 1.2.0 Fixed**（#10765）：`extensions` / `skills` / `prompts` / `themes` 设置里
`!pattern`、`+path`、`-path` 的 `~` 不展开，`!~/skills/pdf` 什么都匹配不到。

**prux 现状**：`src/core/skills.rs:411-422` `parse_filter` 只剥前缀、去前导 `/`，**不展开 `~`**；
`filter_matches_skill`（`:441-460`）用绝对路径 `full` 等候选匹配，`!~/skills/pdf` 永不命中。
对照：主题路径**已**展开 `~`（`src/modes/interactive/theme.rs:345-355`、`:420-427`）。
（prux 的 extensions / prompts 没有这套 glob 语法，不涉及。）

**建议**：`parse_filter` 里对 pattern 调 `utils::paths` 的 `~` 展开；补一条命中/不命中的回归。

### 1.11 🔧 恢复会话不恢复会话内的 active tools

**pi 1.3.0 Fixed**（#10785）：恢复会话时把会话的 active tools 恢复回来，
`--tools +name` / `-name` 作用在其之上；只有显式 `--tools 名字…` / `--no-tools` / `--no-builtin-tools`
仍然整体覆盖。

**prux 现状**：`src/main.rs:271-296` 纯 `+name`/`-name` 的基线取 `read_settings_default_tools()`
或 `DEFAULT_TOOL_NAMES`，**不读会话记录**；`Agent::new`（`src/core/agent_session.rs:992-1141`）
用 CLI 的 `selection` 组工具。会话侧数据其实齐全：
`session_manager.rs:1703` 的 `last_active_tools()` 与 `session_v4.rs:1830/1850` 的
`active_tool_names`。

**建议**：恢复会话且未给显式 allowlist 时，把修饰符作用在 `session.last_active_tools()` 之上；
显式选择仍覆盖。

### 1.12 🔧 压缩 / 分支摘要 / bug report 的对话序列仍含 thinking

**pi 1.2.0 Changed**：`serializeConversation()`（压缩、分支摘要、bug report 共用）不再输出 thinking 块——
replay 出来的 thinking 常常不在当前上下文里，带上它会让摘要请求比被摘要的上下文还大，直接失败。

**prux 现状**：`src/core/compaction/utils.rs:47-96` `serialize_conversation` 对 assistant 显式拼
`"[Assistant thinking]: {}"`。三处调用点：`agent_session.rs:1494`（压缩）、`:1831`（分支摘要）、
`src/core/bug_report.rs:425`（bug report）。

**建议**：删除 `utils.rs:84-96` 的 thinking 收集与 push；同步改
`tests/compaction.rs:106-121`（现断言必须含 thinking）。

### 1.13 🔧 codemode 非首行的 `// @options:` 被静默忽略

**pi 1.2.0 Fixed**（#10631）：`parseCodemodeSource()` 此前忽略不在首行的 `// @options:`，
`timeout_ms` 从不生效；现在直接抛错。

**prux 现状**：`src/extensions/codemode/source.rs:104-127` `parse_source` 只把**首行**当 options，
其余行的 `// @options:` 是普通 JS 注释 → 静默忽略。测试 `only_first_line_is_an_options_line`
（`:184-192`）断言的正是这个旧行为。

**建议**：扫描其余行，命中 `// @options:`（trim 后前缀匹配）即 `Err`，在脚本运行前失败；
把该测试改为断言报错。

### 1.14 🔧 键位 `+` / `ctrl++` 无法表达

**pi 1.2.0 Fixed**（#9880）：Kitty 键盘协议 / modifyOtherKeys 下，名字里含 Shift 符号
（`+`、`ctrl++`）的绑定匹配不上；`parseKey()` now 返回**产出的符号**（`+` 而非 `shift+=`）。

**prux 现状**：匹配侧已正确——`keybindings.rs:131-153` 的 `shift_upper('=') == '+'`，
`:269-272` 同时接受 kitty raw（`Char('=')+SHIFT`）与终端转换后的 `Char('+')`。
但**配置解析**坏了：`:206-215` `parse_key_id` 用 `s.split('+')` 取最后一段当 base，
`"+"` → `["",""]`、`"ctrl++"` → `["ctrl","",""]`，base 都是空串 → `parse_base` 返回 `None`，
绑定被静默忽略（默认表因此只能用 `ctrl+-` 这类符号键）。

**建议**：`parse_key_id` 改用 `rsplit_once('+')` 或对末尾连续 `+` 归并成一个字面 `+`，
使 `+` / `ctrl++` 可解析；`KeyId` 的四布尔模型无需改。

### 1.15 🔧 `--model` / `--models` 多 provider 歧义未处理

**pi 1.3.0 Fixed**（#10160）：多个 provider 都有同一个模型时，`--model` 此前按目录顺序选；
现在**优先有凭据的 provider**，否则报歧义。`--models` / `enabledModels` 则为每个提供该模型的
provider 各加一条并 warn 歧义。

**prux 现状**：`src/core/model_resolver.rs:1170-1216` `resolve_provider_model` 只看模型串有没有
provider 前缀；裸模型名在 `(None, None)` 分支落到 `SUPPORTED_PROVIDERS[0]` 的默认模型上
（**连名字都被忽略**）。`--models` / `enabledModels` 走 `src/core/model_scope.rs:124-215`：
`find_exact` 遇歧义返回 `None`，随后 `fuzzy_single_match` 按 id 降序取**单个**，
既不为每个 provider 各加一条，也不产生歧义诊断（只有无匹配 / 非法 thinking 档两种诊断）。

**建议**：`resolve_provider_model` 的裸模型名分支改为「按凭据优先，否则报歧义」；
`model_scope` 的歧义路径改为「每个 provider 各推一条 + 一条 `Ambiguous` 诊断」，面板/CLI 显示警告。

### 1.16 ⚠️ read 行范围标签（数字字符串 / null，且缺 offset）

**pi 1.3.0 Fixed**（#9887/#10938）：TUI、session tree、HTML 导出共用一个
`formatReadLineRange`——宽松解析数字字符串与 null，`offset` 缺省为 1，输出 `:start-end`。

**prux 现状**：`src/modes/interactive/render/messages.rs:1919-1921` 只显示 `(limit N)`，
用严格 `as_i64()`（`"5"`、`null` 一律不显示），并且**根本不显示 offset**；
`export_html` 不渲染该标签。

**建议**：属显示口径，非功能 bug。若要严格对齐 pi，实现一个共享的
`format_read_line_range(offset, limit) -> Option<String>`（宽松解析、`offset==None → 1`、
输出 `:start` / `:start-end`），TUI 与 HTML 导出共用。当前 prux 的 `read` 调用头格式本就与 pi
不同（`(limit N)` vs `:start-end`），是否改口径需先定。

### 1.17 ⚠️ codemode 描述声称「所有失败都 reject」（行为已对，文案待改）

**pi 1.2.0 Changed**：声明了 output schema 的工具，失败时也**resolve** 自己的值
（例如 Bash 非零退出），描述不再说「每个失败的调用都 reject」。

**prux 现状**：行为**已经是对的**——`src/extensions/codemode.rs:323-331` `script_value`
对「声明了 `output_schema` 且有结构化结果」的工具（错误结果同样适用）返回结构化值，
只有无 schema 的失败才抛错。但 `src/extensions/codemode/description.rs:67` 的 INTRO 仍写
``tools.<name>(args) ... rejects with an Error ... on failure``，与实际不符。

**建议**：改这一句描述即可（模型可见文案）。

### 1.18 ⚠️ codemode 中断是否覆盖 promise 排水路径

**pi 1.2.0 Changed**（codemode 包）：VM 在排空 promise job 时也轮询中断，
于是 `while (true) await null` 与同步死循环在同样的点停下。

**prux 现状**：`src/extensions/codemode/sandbox.rs:230-234` 用 rquickjs
`Runtime::set_interrupt_handler`（`cancel || 超墙钟`）打断；现有测试只覆盖
`await new Promise(() => {})`（`:1120-1136`）与取消标志（`:1143-1165`），
没有 `while (true) await null`。

**建议**：补一条 `while (true) await null` + `timeout_ms` 的用例；若 rquickjs 的中断处理器
在排空 job 时确实不触发，再考虑在胶水层插入让出点。先测后改。

---

## 二、本区间新增 / 变更功能（候选移植）

### 2.1 🆕 `/share` 的上传目标可配置（`sessionShareDestination`）

**pi 1.3.0 New Feature**（#10870）：新增设置项，取值 `auto` / `gist` / `radius`，
决定 `/share` 往哪里传。

**prux 现状**：`/share` **已存在**——`src/modes/interactive/handlers/commands.rs:73`、
`src/modes/interactive/worker.rs:524-573` `dispatch_share`，但硬编码
`gh gist create --public=false`；全仓无 `sessionShareDestination`，无 Radius
（`src/core/login_registry.rs:11` 备注 Radius 暂不实现）。

**建议**：在 `settings_manager` 加该字段 + `/settings` 项，`dispatch_share` 按取值分支。
Radius 依赖对应 provider，可先只做 `auto` / `gist`（`auto` 在无 Radius 时等同 gist 并提示）。

### 2.2 🆕 `PI_CLIPBOARD_OSC52` 强制走 OSC 52

**pi 1.2.0 New Feature**（#10826）：本地剪贴板写成功后，若 `PI_CLIPBOARD_OSC52=1`
再向终端发一次 OSC 52，使复制能到达「检测不到远程会话」的远端客户端（多路复用器 attach、
远端 IDE daemon）。

**prux 现状**：`src/utils/clipboard.rs:51-73` `write_clipboard` 只在
`is_remote_session()` / `is_headless_linux()` 时把 OSC 52 当**兜底**；本地写成功即 `return Ok`。
无该环境变量（全仓 grep 无命中），`src/extensions/` 也无同名扩展。

**建议**：本地写成功后，若 `PI_CLIPBOARD_OSC52=1` 再追加一次 `write_osc52`（失败忽略，
不影响返回值）。注意语义差异：pi 是「追加」，prux 现在是「二选一」。

### 2.3 🆕 配置文件的 `$schema` 与启动期校验

**pi 1.2.0 New Feature / Breaking**（#9880/#10751）：`settings.json`、`models.json`、
keybindings、themes 都带 `$schema`（`https://pi.dev/schemas/*.schema.json`），
主题 schema 移到 `.../theme.schema.json`；**启动时加载的主题也做同样的校验**；
可复用颜色定义在 `vars` 下；不支持的 metadata 要移除。

**prux 现状**：无任何 `$schema` 或 schema 校验（全仓 grep 无命中；keybindings 的非法项
静默丢弃，`src/core/keybindings.rs:742-743`）。主题 `vars` **已支持**——
`src/modes/interactive/theme.rs:50-51` 的字段与 `:513-514` 的 `ingest_section(v.get("vars"))`、
`:488-506` 的合并规则。

**建议**：`vars` 无需改。若要对齐，至少给默认 settings / models / keybindings / theme 模板
补 `$schema` 字段；启动期校验可后置（prux 现在对非法配置普遍是静默忽略或给默认值）。

### 2.4 🆕 OpenRouter 按 key 定制模型目录（`GET /models/user`）

**pi 1.2.0 New Feature**（#10353/#10672）：持 OpenRouter key 时只列该 key 可用的 chat 模型，
带该 key 的上下文长度 / 最大输出 / 价格（含 `models.json` 里设置的区域端点），
结果保存下来离线可用；ai 侧新增 `personalizeOpenRouterCatalog()` 与
`RefreshModelsContext.baseUrl`。

**prux 现状**：`src/core/model_refresh.rs:26` 固定 `CATALOG_BASE_URL = "https://pi.dev"`，
`:159-165` 统一拉 `/api/models/providers/{provider}?types=…`；无 `/models/user`，
无 baseUrl 刷新上下文。

**建议**：给 openrouter 加专属分支（持 key 时打 `/models/user`），刷新上下文带上
`models.json` 里的 base URL 覆盖。需保留「离线时回退到已保存目录」的现有行为。

### 2.5 🆕 codemode 声明标注输入约束（`@integer` / `@minimum` / …）

**pi 1.2.0 Changed**（#10707）：`schemaToType()` 把 TS 类型表达不了的约束写成属性注释行上的
`@` 标签（`// @integer @minimum 0 @maximum 30000 @default 0`），覆盖 `integer`、数值上下界、
`multipleOf`、字符串长度、`pattern`、`format`、数组长度、`uniqueItems`、非 null `default`、`deprecated`；
可空 `anyOf`/`oneOf` 的约束取非 null 分支。

**prux 现状**：`src/extensions/codemode/declarations.rs:33-52` `schema_to_type` 只渲染 TS 类型，
全仓无这类 `@` 标签；超过 `DEFAULT_INPUT_SCHEMA_MAX_CHARS`（`:13`）时退化 `unknown` 的逻辑已有。

**建议**：在属性注释行后追加约束标签，与 pi 保持同一套关键字与格式（这是模型可见文案，
会影响脚本可靠性）。

### 2.6 🆕 `reachesTools`（工具「能触及哪些工具」的声明）

**pi 1.2.0 Added**：工具定义新增 `reachesTools`，`codemode` / `tool_search` 借此声明一次调用
可能触达的工具；MCP 扩展用它等「这次调用需要的 server」连接完，并判断被替换的
`codemode` / `tool_search` 是否还能触达 MCP 工具。

**prux 现状**：无该字段（`ExtensionTool` 见 `src/core/extensions/tools.rs:400-434`；
装载信息只有 `ToolLoadout{declared, callable}`）。prux 的 MCP 是**单一网关工具**
（`src/extensions/mcp.rs:255-263`、`:631-660`），调用时惰性连接（`mcp/manager.rs:703`），
不存在「等某次 call 所需的 server」问题；工具替换表现为 `prepare_loadout` 改写描述
（`src/extensions/codemode.rs:144-172`），不是替换工具本身。

**建议**：无消费点，可选。若将来 MCP 改成按 server 注册独立工具，再补该字段。

### 2.7 🆕 工具命名空间的 `aliases`

**pi 1.2.0 Added**：命名空间支持 `aliases`；codemode 的 `describeNamespace()` 与
`searchTools({ namespace })` 会匹配别名；**MCP 命名空间用 server 名**。

**prux 现状**：`namespace` 是纯字符串 `Option<String>`（`core/extensions/tools.rs:420`），
无 `aliases`。codemode 侧用 `namespace_key()` 归一化（去 `mcp__`/`mcp_` 前缀、`-` 视作 `_`，
`codemode.rs:830-837`），`describe_namespace` 与 `searchTools` 的 namespace 选项都走它
（`:421-433`、`:363-381`）；`tool_search::tool_document` 把 namespace 拼进检索文本
（`src/extensions/tool_search.rs:228-230`）。MCP 网关工具的 `namespace` 为 `None`
（`src/extensions/mcp.rs:644`），即没有「以 server 名作命名空间」。

**建议**：prux 现无多命名空间来源，可选；归一化函数已覆盖 pi 别名的一半场景。

### 2.8 ⚠️ `mcp list --json` 报 `resources` 布尔

**pi 1.2.0 Added/Breaking**（#10526）：`pi mcp list --json` 对有 resources 的服务器报
`resources: true`，不再报 `resources` / `resourceTemplates` 计数；连接时**不再列 resources**。

**prux 现状**：「连接时不列 resources」已满足——`mcp/manager.rs:628-664` 的 `refresh_tools`
只调 `list_all_tools()`，resources 仅在显式 `list_resources()`（`:443-485`）时拉取，
所以不存在「resource 没列完就不注册工具」。但 `src/cli/mcp_command.rs:235-252` 的
`--json` 输出只有 `{name, server, sources}`，且 `list` **不连接**服务器（无法得知是否 offer resources）；
`/mcp status` 走缓存计数（`mcp.rs:1155-1200`）。

**建议**：可选。若要布尔字段，需先决定 `list` 是否连接探测（架构取舍）；否则恒 `false` 反而误导。

### 2.9 🚫 OpenAI GPT-6 中途改 thinking level 保持 prompt cache

**pi 1.3.0 New Feature**（#10853）：openai provider + API key 时，中途改 thinking level
以 `configuration_update` 输入项下发，而不是改 `reasoning.effort`，从而保住 prompt cache；
由 `compat.supportsMidConvoEffort` 控制。

**prux 现状**：无 `configuration_update` / `supportsMidConvoEffort` 代码；
`compat_flag`（`src/core/provider/convert.rs:44-52`）只读与系统消息相关的 flag。
该簇在上一轮已被定为**决定不实现**——见 `migrations/migration-v1.1.0.md` 2.16 残留 A
与第十三节。

**建议**：维持不实现（决策沿用）。若将来翻案，需按 pi 以 `configuration_update` 输入项下发。

### 2.10 ✅ Kimi Code 走 Chat Completions（数据面已满足）

**pi 1.3.0 Changed**（#10720）：`kimi-coding` 改用 OpenAI Chat Completions
（`https://api.kimi.com/coding/v1`），K3 / K3-256K / Kimi For Coding 支持中途加工具不失效缓存；
`models.json` 覆盖 base URL 时必须带 `/v1`。

**prux 现状**：数据面已满足——`assets/models/providers/kimi-coding.json` 全部
`api = openai-completions`、`baseUrl = https://api.kimi.com/coding/v1`。
**附注**：`src/core/login_registry.rs:125` 的登录元数据 `base_url` 仍是不带 `/v1` 的
`https://api.kimi.com/coding`（未见请求路径消费该字段），可顺手对齐以免将来踩坑。

### 2.11 ✅ Xiaomi MiMo 的 thinking 档位（数据面已满足）

**pi 1.3.0 Fixed**（#10818）：MiMo 在 `minimal` 档 400，pi 只发文档化的 on/off 开关，
只暴露 `off` 与 `high` 两档。

**prux 现状**：数据面已满足——`assets/models/providers/xiaomi.json:15-23` 的 `thinkingLevelMap`
只有 `off`/`high`，其余为 `null`；代码走 deepseek 格式只发 thinking 的 enabled/disabled
（`src/core/provider/completions.rs:856-866`），`src/core/provider.rs:374-392` 也只暴露 off/high。

### 2.12 ✅ Claude Haiku 5.5 / Opus 5 的 `off` 档（数据面已满足）

**pi 1.3.0 Fixed**（#10753）：这两个模型不支持 `off`；pi 改为发
`thinking: {type: "disabled"}` 且不带 effort。

**prux 现状**：`src/core/provider/anthropic.rs:888-898` 的关闭档判定
`should_disable = model.thinking_level_map.get("off").is_none_or(|v| v.is_some())`，
命中时发 `{"type":"disabled"}` 且不写 `output_config.effort`；
`assets/models/providers/anthropic.json` 里这两个条目没有 `off` 键。

### 2.13 ✅ OpenCode / OpenCode Go 端点迁移（数据面已满足）

**pi 1.3.0 Fixed**：OpenCode Zen / Go 迁到 `opencode.ai/inference`（Go 的 Gemini 404 修复、
Go 加 Google Generative AI 支持、Go 的 MiniMax M2.7 改用 Anthropic Messages API）。

**prux 现状**：数据面已是 `https://opencode.ai/inference/v1` 与 `.../go/v1`，
provider 的 `api` 由目录条目决定，无需代码改动。

### 2.14 ✅ codemode 行显示 elapsed / total

**pi 1.2.0 Added**（#10708）；**pi 1.2.0 Fixed**（#10678，多 `text()` 拼接的二次复杂度）。

**prux 现状**：`src/modes/interactive/render/messages.rs:1639-1662` 的 `render_tool_output`
运行中显示 `Elapsed`、完成显示 `Took`（`durationMs` 由 `codemode.rs` 回填）；
`src/extensions/codemode.rs:1162-1177` 用 `push_str` 摊还 O(n)，`glue.js` 的 `output()`
逐条回调不做字符串累加。两条都已满足。

### 2.15 ✅ `showHardwareCursor` 只画终端光标

**pi 1.2.0 Breaking**：启用时不再额外画反色假光标（避免终端把边缘格颜色扩到窗口 padding）。

**prux 现状**：根本没有该设置项（`src/modes/interactive/settings_selector.rs:672-673`
明确把 hardware cursor 列为不支持），光标只由 `frame.set_cursor_position` 设置
（`render.rs:421-512`），`render/input.rs:210-221` 无反色假光标。已是 1.2.0 之后的行为。

### 2.16 ❌ `before_agent_start` 覆盖扩展触发的回合 + `messageType` 联合体

**pi 1.2.0 New Feature / Breaking**（#10267/#5581/#10739）：`pi.sendMessage(..., {triggerTurn:true})`
空闲时启动的回合也走同一套前置设置；`BeforeAgentStartEvent` 变成按 `messageType` 区分的联合体
（`"user"` 带 `prompt`/`images`，`"custom"` 带起始 `message`）；会话在回合**启动期间**即算忙。

**prux 现状**：无 `before_agent_start` 钩子（`src/core/extensions/hooks.rs:9-62`；
`migrations/migration-tasks.md` 已记「无直接对应」）。提交通知走 `on_user_prompt`
（`src/core/extensions.rs:542`）/ `on_user_submit`（`:567`），载荷是 `text` + `busy`，无 `messageType`。
扩展自主续跑走 `ExtensionUiRequest::Continuation`，TUI 侧**忙碌即丢弃**（`modes/interactive.rs:1169-1176`），
空闲才入队。system prompt 的段落在 prux 每次 LLM 调用重算（`agent_loop.rs:385-391`、`:484-506`），
不存在「首轮工具调用后被清除」。

**建议**：无该事件，不适用。若将来希望「忙碌时排队而不是丢弃」，改动点在
`modes/interactive.rs:1171`。

### 2.17 ❌ `ctx.ui.custom({overlay:true}).done()` 关错层

**pi 1.3.0 Fixed**（#10648/#10999）：此前关掉的是栈顶 overlay，导致自己的 overlay 留在屏幕上、
另一个扩展的对话框永远不结束。

**prux 现状**：只有单槽 `App.overlay: Option<OverlayState>`（`src/modes/interactive/app.rs:595`），
打开新层先给旧层发 `Closed` 再替换（`overlay.rs:181-191`）；扩展关层走 `HideOverlay{id}`，
`modes/interactive.rs:1160-1164` **先校验 `o.id == id` 才关**。单层模型下「关栈顶」=「关自己」，
且已按身份校验。

### 2.18 ❌ tui 的 `renderFakeCursor()` / APC 标记 / overlay 合成的 `CURSOR_MARKER`

**pi 1.2.0 Breaking/Added/Fixed**：`Editor.render()` / `Input.render()` 改用零宽 APC 标记包裹
光标；新增 `renderFakeCursor()`；修复 overlay 合成丢掉 `CURSOR_MARKER`。

**prux 现状**：没有 pi 的组件层，也没有 `CURSOR_MARKER` / APC 概念（`render/input.rs` 由内核
直接算光标坐标）。结构性不适用。

### 2.19 ❌ pi-codemode 库新增 sandbox 形态与确定性重放

**pi 1.2.0 / 1.3.0**：新增 `RemoteCodemodeSandbox`（VM 在别的 isolate）、
`InlineCodemodeSandbox`（调用线程 + `interruptBudget`）、`portable` 入口、
`CodemodeSandboxBase`、`sandbox.start()` 确定性重放（虚拟时钟 / 种子随机 / `onCall` / `deliver`）、
`onWorkerStarted(exited)` 与 `acquireWorker()` 容量钩子；worker 关闭改为「请求关闭即结算」。

**prux 现状**：`src/extensions/codemode/sandbox.rs` 是自有 rquickjs 实现，单进程、无线程 worker、
无桥接协议；这些是给宿主嵌入的库 API，prux 无对应层。结构性不适用。
唯一可对照的是 `interruptBudget` ≈ prux 的墙钟上限（`MAX_WALL_MS`）。

### 2.20 ❌ ai 包移除 `AssistantMessageFrame` / `reduceAssistantMessageFrames()`

只被已移除的 experimental agent harness 使用；prux 无对应类型。

### 2.21 ❌ durable 的系统基线位置、env 的 `listFiles` / pty、chord 全套、telemetry

见第五节（整段不列出）。

---

## 三、行为/接口变更（对齐时容易踩的坑）

| # | 变更 | prux 影响 |
|---|---|---|
| 3.1 | MCP `CallToolResult.structuredContent` 由 `Record<string,unknown>` 放宽为 `unknown`（允许数组 / `null` / 标量，#10416） | **✅ 已满足**：prux 走 `rmcp 3.1.4`，其 `structured_content: Option<serde_json::Value>` 本就是任意 JSON；`src/extensions/mcp.rs:759` 原样透传，全仓无「必须是 object」的校验；codemode 侧无 output schema 时声明为 `unknown`（`declarations.rs:43-52`） |
| 3.2 | MCP 连接时不再列 resources；`mcp list --json` 报 `resources: true`（#10526） | **✅ 连接面已满足**（`mcp/manager.rs:628-664` 只列 tools）；**⚠️ `--json` 形状**见 2.8 |
| 3.3 | MCP 请求在 OAuth 刷新期间被取消/超时后仍会发出 → 新增 `signal`（#10598） | **✅ 已满足**：prux 的 `wait_request` 用 `tokio::select!`（`mcp/manager.rs:1189-1215`），取消即 drop 整条 RPC future，rmcp 的 token 刷新与发送在同一 await 链内，不会补发。`oauth.rs:484-507` 的刷新请求刻意不按 token 取消，仅是为避免持跨进程刷新锁时弃锁 |
| 3.4 | MCP OAuth 授权码交换校验 `iss`（RFC 9207，无 metadata 时 `expected: undefined`） | **✅ 已满足**：由 rmcp 提供（`validate_authorization_response_issuer`，`AuthorizationServerMismatch`）；prux 侧开关映射到 `skip_issuer_metadata_validation`（`oauth.rs:866`）。错误类型名与 pi 不同，语义等价 |
| 3.5 | Mistral 请求超时只作用于「等响应头」，不再中断生成中的流（#10609） | **❌ 不适用**：prux 无 Mistral provider |
| 3.6 | Windows 上 `auth.json` 的锁目录仍在删除导致 `EPERM` 时短暂重试（#10801） | **❌ 不适用**：prux 无跨进程锁目录——`auth.rs:227-237` 是临时文件 + `rename` 的原子写，只有进程内 `RwLock`；非 unix 的文件锁是 `hard_link` 到同名 sidecar **文件**（`utils/file_lock.rs:263-313`），无「上一个锁文件夹正在删除」的竞态 |
| 3.7 | `pi mcp login --timeout` 限制整段登录（帮助文案同步更正，#10636） | **🚫 决定不实现**（沿用上轮结论，见 `migrations/migration-v1.1.0.md` 2.15 / 第十三节）；prux 的 `mcp login` 无 `--timeout`（`src/cli/mcp_command.rs:54`） |
| 3.8 | print 模式 `-p` 最终回答被输出上限截断时改为「打部分文本 + stderr 警告 + 退出码 1」（#10799） | **❌ 不适用**：prux 无 `-p` / print 模式（`src/cli/args.rs` 无该参数，入口只有 `interactive::run_interactive`） |
| 3.9 | 终端配色回复分片时 hex 值泄漏进 prompt / 其 BEL 触发外部编辑器（#10256/#10657） | **✅ 已满足**：`utils/terminal_colors.rs:139-145` 的查询串以 DA1（`\x1b[c`）作哨兵；`:290-343` 跨多次 `read_available` **累积缓冲**；`:146-201` 的 `take_reply` 对未完成序列返回 `None` 且不消费；查询在事件流建立之前完成（`modes/interactive.rs:200`、`:207`） |
| 3.10 | 全屏 overlay 阻断滚轮滚动 transcript（#10769） | **✅/不适用**：prux 的滚轮先判 `overlay::captures_mouse`（`handlers/mouse.rs:80-97`），全屏 overlay 恒捕获（`overlay.rs:434-441`，此时消息区本就不渲染）；`OverlayEvent` 无 Mouse/Wheel 变体，滚轮**永不**作为 raw input 回传扩展 |
| 3.11 | 内联图片在 Herdr 不渲染 → Herdr 改用 Kitty graphics（#10774） | **❌ 不适用**：全仓无 Herdr（上轮指南记 Herdr 是 OSC 8 超链接场景）。prux 的图片协议走 `Picker::from_query_stdio()` + `PRUX_IMAGE_PROTOCOL` 覆盖（`utils/terminal_image.rs:22`、`:166-191`）。**可选**：落到 `env_protocol` 白名单路径时（`:332-353` 只列 kitty/ghostty/wezterm/warp/iterm2），Herdr 会退到 halfblocks |
| 3.12 | 自定义编辑器里的压缩状态指示器用默认 muted 色（#10809） | **❌ 不适用**：prux 无「自定义编辑器」概念；压缩状态在状态栏统一渲染（`render/status.rs:42-69`，`WorkingKind::Compaction`） |
| 3.13 | codemode 在 `node --watch` 下桥接损坏（#10725）/ Bun 上 worker 不退出（#10748） | **❌ 不适用**：Rust 单进程、无 worker 与桥接协议 |
| 3.14 | 模型切换丢弃其它模型的 thinking；摘要请求不含 thinking；kimi-coding base URL 必带 `/v1` | 见 1.8 / 1.12 / 2.10 |
| 3.15 | 主题 schema 迁移（`$schema` URL、`vars`、移除不支持的 metadata）、启动期主题校验 | 见 2.3 |

---

## 四、模型目录 / 数据同步（与代码改动解耦）

1. 本轮数据面**已完成**（提交 `b392431`）：`assets/models/**` 新增 102 个模型、移除 40 个，
   涉及 `github-copilot` / `google` / `huggingface` / `meta` / `nvidia` / `opencode` /
   `opencode-go` / `openrouter` / `vercel-ai-gateway`。
2. `SUPPORTED_PROVIDERS`（`src/core/model_resolver.rs:31`）**本次不变**，仍 35 个：
   pi 的 `openai-codex` 不在其中（`model_resolver.rs:2067` 有断言），所以
   「`openai-codex/gpt-6.1-sol` 用 272K 而非 500K 窗口」（#10840）在 prux 上不适用；
   prux 的 `gpt-6.1-sol` 属 `openai` provider，上下文长度随目录同步即可。
3. OpenRouter 的负价占位（#10672）在 prux 目录里当前不存在（cost 最小值为 0），无需处理。
4. 同步后跑 `scripts/sync-models.py --check` 验证零差异，再跑 `cargo test`。
5. `sync.md` 已是 `1.3.0`，无需再改。
6. **`assets/changelog/v.1.3.0.md` 目前是空文件**（AGENTS.md 要求它与 `Cargo.toml` 版本绑定、
   每次提交检查是否该更新），发版前必须补内容；1.3.0 的用户可见条目可直接由本文件一、二、三节改写。

---

## 五、整段不列出的 pi 功能（prux 无对应实现）

以下在 changelog 里有条目，但 prux 没有对应功能，按收录规则不逐条展开（改动内容仅供将来接入时参考）：

- **`packages/chord`**（1.2.0 新建，1.3.0 有大量破坏性变更）：typed host runtime、
  facet / link / deflate / flow control / protocol 4。prux 是单进程单 crate，无插件宿主模型，
  结构性不适用；且 prux 上一轮已把「不依赖 chord」当既成事实。
- **`packages/{client,protocol,server}`**：1.2.0 随 experimental 栈一并**移除**；prux 从未移植。
- **`packages/durable`**（早于区间）：本区间有系统基线位置（#10904）、`toBase64` 内存（#10923）、
  tool task 标签联合体、structured tool output、deferred tools、nested calls、scheduler 性能与内存、
  document cache 保留期等大量条目。prux 没有 durable/harness 层。
- **`packages/env`**（早于区间）：本区间新增 `listFiles`、`LocalExecutionEnv`、`nodePlatform`、
  pty / `RemoteTerminal` / `foreground`。prux 无 env daemon。
- **`packages/telemetry`**（早于区间，本区间无用户可见条目）。
- **Node / npm / Nix 打包面**：内部依赖精确版本锁定（#10865）、Nix flake 的 catalog pin、
  Nix 的 Node 只作 fallback（#10519）、Nix install check、`node --watch`、Bun worker。
- **无对应 provider**：Amazon Bedrock、Mistral、Radius、llama.cpp、`openai-codex`
  （含 `LoginOptions.agentName`、originator 头）。
- **pi-ai 的库类型层**：`AssistantMessageFrame` 系列移除、`AssistantMessageFrameEncoder` 等，
  只服务已移除的 experimental harness。
- **pi-codemode / pi-mcp / pi-tui 的库 API**：`McpTransport.send(signal)`、
  `renderFakeCursor()`、`CURSOR_MARKER`、APC 标记、`RemoteCodemodeSandbox` /
  `InlineCodemodeSandbox` / `portable` / `sandbox.start()`，以及 `Box.setPaddingX()` 这类组件 API。
- **扩展 API 的 pi 侧语义**：`before_agent_start`、`pi.sendMessage`、
  `ctx.ui.custom({overlay:true})` 的栈语义——prux 没有这套扩展宿主 API（见 2.16–2.17）。

---

## 六、落地检查清单

- [x] `assets/changelog/v.1.3.0.md` 补内容（中英两段，已按本文件的一～三节改写）
- [x] `RETRYABLE` 补三条重试串 + 断言（1.1）
- [x] 非法 `\u` 转义修复（1.2）
- [x] 畸形参数不静默执行：`arguments_error` + 原文回给模型（1.3）
- [x] Anthropic error tool result 的图片移到回合末（1.4）
- [x] 同 chunk 内 thinking 排在 text 前（1.6）
- [x] Gemini 4 走 `thinkingLevel`（1.7）
- [x] 切模型时丢弃其它模型的 thinking（1.8）
- [x] prompt 模板空引号位置参数（1.9）
- [x] skills 过滤条目展开 `~`（1.10）
- [x] 恢复会话/切换会话装回 active tools（1.11）
- [x] `serialize_conversation` 去掉 thinking（1.12）
- [x] codemode 非首行 `@options` 报错（1.13）
- [x] 键位 `+` / `ctrl++` 可解析（1.14）
- [x] `--model` / `--models` 歧义策略（1.15）
- [x] codemode 描述不再声称「所有失败都 reject」（1.17）
- [x] 补 `while (true) await null` 中断用例（1.18，结论：已满足）
- [x] 改动完成后：`cargo fmt`、`cargo test`（3602 + 集成全绿）、`cargo clippy`
- [ ] （可选）`login_registry.rs:125` 的 kimi-coding `base_url` 补 `/v1`（2.10 附注）
- [ ] （可选）Anthropic `strict` 与回落重试一起移植（1.5，现无 strict 故无此 bug）
- [ ] （可选）read 行范围标签统一口径（1.16）
- [ ] （可选）`sessionShareDestination`（2.1）
- [ ] （可选）`PI_CLIPBOARD_OSC52`（2.2）
- [ ] （可选）配置 `$schema` 与启动期校验（2.3，主题 `vars` 已支持）
- [ ] （可选）OpenRouter `/models/user` 定制目录（2.4）
- [ ] （可选）codemode 声明约束 `@` 标签（2.5）
- [ ] （可选）`reachesTools` / `aliases` / `mcp list --json` 形状（2.6–2.8）
- [x] 🚫 不实现项记录在案：`supportsMidConvoEffort`（2.9）、`mcp login --timeout`（3.7）

---

## 七、实施记录（bug 修复 + `parse_key_id`，已完成）

本轮只做第一节的 bug 修复；新增能力（第二节 🆕）与可选项未开工。

| # | 项 | 改动位置 | 关键测试 |
|---|---|---|---|
| 1.1 | 三条瞬时错误可重试 | `core/agent_session.rs` `RETRYABLE` | `retryable_error_classification_matches_pi` |
| 1.2 | 非法 `\u` 转义修复 | 新增 `core/provider/json_args.rs`（`repair_json` / `parse_streaming_json`）；`completions` / `anthropic` / `responses` 改从该模块取用 | `json_args::tests::repairs_invalid_unicode_escapes_without_dropping_fields`、`truncated_unicode_escape_keeps_earlier_fields` |
| 1.3 | 畸形参数不静默执行 | `json_args::parse_tool_call_arguments`；`ContentBlock::ToolCall.arguments_error`（新增字段，serde 可选）；`agent_session::prepare_tool_calls` 直接失败 | `json_args::tests::finalized_arguments_*`、`malformed_tool_arguments_fail_before_execution` |
| 1.4 | Anthropic error 结果的图片 | `anthropic.rs`：`ERROR_TOOL_IMAGE_PLACEHOLDER`、`split_error_tool_result_images`、`flush_error_tool_result_images` | `error_tool_result_images_move_after_the_tool_results` |
| 1.6 | thinking 排在 text 前 | `completions.rs::handle_delta` 先 reasoning 后 content | `reasoning_precedes_content_when_one_chunk_carries_both` |
| 1.7 | Gemini 4 `thinkingLevel` | `google.rs::is_gemini4_or_later` 纳入 `uses_google_thinking_level` | `uses_google_thinking_level_matches_pi_patterns` |
| 1.8 | 切模型丢弃 thinking | `anthropic.rs` / `completions.rs` / `responses.rs` / `google.rs` 四套 convert 都按 `is_same_model` 丢弃 | `empty_thinking_signature_replayed_for_compat_models`、`thinking_cross_model_is_dropped`、`cross_model_thinking_is_dropped`、`thinking_signature_is_passed_through` |
| 1.9 | 空引号位置参数 | `prompt_templates.rs::parse_command_args` 加 `started` 标志 | `keeps_empty_quoted_arguments` |
| 1.10 | 过滤条目展开 `~` | `skills.rs`：`normalize_filter_pattern` + 新增绝对父目录候选 + 精确匹配两侧归一 | `filter_expands_tilde_in_paths` |
| 1.11 | 恢复会话的 active tools | `agent_session.rs::restore_session_tools`；`main.rs` 与 `worker.rs::attach_session` 调用 | `restore_session_tools_restores_recorded_tools`、`restore_session_tools_keeps_explicit_selection` |
| 1.12 | 摘要不含 thinking | `compaction/utils.rs::serialize_conversation` | `tests/compaction.rs::serialize_conversation_drops_thinking_but_keeps_tool_calls` |
| 1.13 | codemode 非首行 `@options` | `codemode/source.rs::parse_source` 扫描其余行报错 | `only_first_line_is_an_options_line` |
| 1.14 | 键位 `+` / `ctrl++` | `keybindings.rs::parse_key_id` 改用 `rsplit_once('+')` + 末尾 `+` 归并 | `parses_modifier_combinations` |
| 1.15 | `--model` / `--models` 歧义 | `model_resolver.rs`：`providers_offering_model` + `pick_provider_for_bare_model`；`model_scope.rs`：`ScopeDiagnostic::Ambiguous` + 每 provider 各推一条；`main.rs` 打印歧义警告 | `bare_model_provider_pick_prefers_credentials_and_reports_ambiguity`、`bare_model_offered_by_multiple_providers_is_added_for_each` |
| 1.17 | codemode 描述文案 | `codemode/description.rs` 的 INTRO（固定开销仍 ≤ 530 token） | `fixed_overhead_stays_small` |
| 1.18 | await 循环可中断 | 无代码改动，只补用例（rquickjs 中断处理器已覆盖 promise 排水） | `awaiting_loop_is_interrupted_by_the_deadline` |

**附加修复（不在 pi 条目内，但阻断 `cargo test`）**：

- `classifier::tests::image_input_requires_a_model_that_accepts_images` 原先断言「目录里的 typesafe
  分类器是纯文本模型」，而本轮同步后的目录给它标了 `input: ["text", "image"]`。该用例要验证的是
  「模型不吃图片时在发请求前拦住」，与目录当前能力无关，故改为显式把 `model.input` 固定为纯文本。

**行为口径变化（升级后用户可见）**：

- 1.2 / 1.3：参数不是合法 JSON 的工具调用**不再执行**，而是产生一条 isError 的 toolResult，
  正文带模型发来的原文；非法 `\u` 转义这类「本意合法」的参数改为修好后正常执行。
- 1.4：带图片的错误工具结果在 Anthropic 上把图片移到本回合 tool_result 之后，并附一句点名工具调用的说明。
- 1.8：换模型后历史里其它模型的 thinking 既不发 thinking 也不降级成文本（此前会降级成文本）。
- 1.11：恢复/切换会话会装回会话记录的 active tools；`--tools +/-` 作用在其上。
- 1.15：`--model <名字>` 在被多个 provider 提供时，以前按目录顺序静默挑一个，现在要么选有凭据的那个、
  要么直接报错；`--models` / `enabledModels` 会为每个 provider 各加一条并打印歧义警告。
- 1.14：`+` / `ctrl++` 以前被静默忽略，现在能解析生效。
- 1.12：压缩/分支摘要/bug report 的序列化不再含 thinking。
- 1.13：codemode 里位置不对的 `// @options:` 以前静默失效，现在整次调用在脚本运行前失败。

