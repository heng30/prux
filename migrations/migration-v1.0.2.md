# Prux 迁移指南：pi 0.99.1 → 1.0.2

> **来源**：pi 仓库 tag `v0.99.1` → `v1.0.2` 之间各包 `packages/*/CHANGELOG.md` 的
> `[0.99.2]`、`[1.0.0]`、`[1.0.1]`、`[1.0.2]` 四个 release 段
> （`git diff v0.99.1..v1.0.2 -- '*/CHANGELOG.md'`）。
> prux 基线：pi **0.99.1**（见 `sync.md` 与 `migrations/migration-v0.99.1.md`）。
>
> **收录规则**（按用户要求）：只收录「prux 已实现功能的 bug 修复」与「0.99.1→1.0.2 区间内
> pi 新增的功能」。prux 没有、且不是本区间新增的功能**不列出**。
> 与上一份指南一致，以下内容整体不适用，直接排除：
> `packages/{client,protocol,telemetry}`（本区间无任何条目）、纯 JS/Node 运行时与打包改动
> （`npm-shrinkwrap`、`brace-expansion`、`pi-ai/models` 入口、`CodemodeSandbox.workerUrl`、
> `pi update` 的 npm 文案、Nix flake）、Kitty 图形协议/终端内联图片（prux 不渲染终端图片，
> 只显示 `[image]` 占位，见 `src/modes/interactive/render/messages.rs:757`）。
> `packages/server` 的 `SessionMetadata` 破坏性变更、`packages/agent` 移除 experimental harness、
> `packages/durable` 新包：prux 是单 crate 架构（能力都在 `src/core/`），三者均标记为结构性不适用。
>
> **状态标记**：🔧 应修/应移植　⚠️ 需对照 pi 源码逐项核对　✅ 已满足/无此问题（附理由）
> ❌ 结构性不适用（仅备注）　🆕 本区间新增、prux 缺失的候选移植项
> 状态行里写「**明确不做**」的条目表示**已决策不再实现**（不是待办），不再计入剩余工作。
>
> **说明**：本文的 1.1–1.30、2.1–2.26 为**对照审计**：正文里的「prux 现状 / 建议」是**落地前**的原文，
> 其中的行号以当时 HEAD 为基准，**不代表现状**。**当前状态以零节与各条目末尾的 `> **状态：…**`
> 为准**，落地记录见第七～十五节。截至最新一轮，零节的开口项已**全部清零**：2.2 / 2.3 / 2.7 / 2.13
> 已落地（第十三节，其中 2.2 只做了「识别 + 报错」第一步），2.11 的「每次请求实时读取 token」与
> 2.19 的 OSC8 超链接也已落地（第十四节，推翻了第十二/十一节当时的「不做」结论）。
> **明确不做**的只剩两项：2.16（regular 逃生开关）、2.2 第二步（federation token 交换）。

---

## 零、总体判断（先看这张表）

| # | 结论 | 数量 |
|---|---|---|
| 🔧 必修 bug 修复（1.1–1.8、1.13、1.14） | **全部落地**（七节） | 10 |
| ⚠️ 需核对（1.9–1.12、1.20、1.22–1.25、1.27–1.30） | 1.9–1.12（八/十节）、1.20/1.25/1.27/1.29/1.30（十节）已落地；1.22–1.24 自判无需改动 | 13 |
| ✅ 已满足 / 无此问题（1.15、1.16；2.20–2.22） | 无需改动 | 5 |
| 🆕 新增候选（2.1–2.19） | 2.1/2.4/2.5/2.6/2.8–2.12/2.14/2.15/2.17/2.18（八/九/十一节）、2.2 第一步/2.3/2.7/2.13（十三节）、2.11 实时读 token/2.19 OSC8（十四节）已落地；**明确不做**：2.16、2.2 第二步 | 19 |
| ❌ 结构性不适用（1.17–1.19、1.21、1.26、2.23–2.26、三节） | 仅备注 | — |

**剩余未做：无。**
- 2.2 只做了**第一步**（识别 + 报错）：真正的 federation token 交换**明确不做**（没接 SDK 要自己实测端点与字段，见 12.1 第二步）。
- 顺带修掉的一个既有 bug：`render/markdown.rs` 的 OSC8 分支会把 `]8;id=none;…` 当文本写进 buffer（第十三节）。
- 明确不做：2.16（prux 本来就默认全屏，缺的只是回滚终端 scrollback 的开关）、
  2.2 第二步（federation token 交换）。
- 曾列入「明确不做」后**推翻并落地**的两项（第十四节）：2.11 的「每次请求实时读取 token」
  （rmcp 的静态 header 可绕：包装 `StreamableHttpClient`）、2.19 的 OSC8 超链接
  （ratatui 0.30 的 `Cell::set_symbol` + `CellDiffOption::ForcedWidth` 能承载转义序列）。
- 第四节的目录全量同步已在第十节完成（`scripts/sync-models.py --check` 零差异）。

---

## 一、Bug 修复（prux 已有功能，升级时应评估移植）

### 1.1 🔧 `Retry-After` 头是不可解析的日期时立即重试（应为指数退避）

**pi 0.99.2 Fixed**（ai #9571）：`retry-after` 解析失败时旧代码得到 `NaN` 并直接交给
「原样使用服务端延迟」的分支，于是退避为 0；现在对 `retry-after-ms` / `retry-after`
都做 `Number.isFinite` 检查，非法值（含 HTTP-date 形式的日期）回落到指数退避；同时支持
`Date.parse(retryAfter) - Date.now()` 解析 HTTP-date。

**prux 现状**：`src/core/provider/retry.rs:54` 的 `retry_after_ms()`

```rust
if let Some(v) = headers.get("retry-after").and_then(|v| v.to_str().ok()) {
    return Some((v.trim().parse::<f64>().unwrap_or(0.0).max(0.0) * 1000.0) as u64);
}
```

解析失败 → `unwrap_or(0.0)` → `Some(0)` → `send_with_retry` 里 `delay_ms = 0` → **立即重试**，
与 pi 修复前的行为一致。且 prux 完全不支持 HTTP-date 形式。

**建议**：`retry-after-ms` 用 `is_finite` 语义（Rust 侧 `is_finite()`）；`retry-after`
先试数值、失败再试 HTTP-date（可用 `httpdate` 或手写解析），仍失败返回 `None` 走指数退避。
补 `retry_after_headers_parse` 的日期用例。

> **状态：已实现（第七节）**。`retry-after-ms` 加 `is_finite()` 判定，`retry-after` 先试数值、
> 失败再试 HTTP-date，仍失败返回 `None` 走指数退避。**位置已变**：实现现在在
> `src/utils/http.rs::retry_after_ms` / `parse_retry_after` / `http_date_ms`（原 `provider/retry.rs`
> 的版本在「重构 http 客户端请求创建」里搬到 utils），`send_with_retry` 经 `http::retry_after_ms` 取值。
> 测试：`retry_after_invalid_values_fall_back_to_backoff`、`retry_after_http_date`。

---

### 1.2 🔧 `"Selected model is at capacity"` 被当成终止错误而非重试

**pi 1.0.1 Fixed**（ai #10278）：`RETRYABLE_PROVIDER_ERROR_PATTERN` 增加
`"model is at capacity"`。

**prux 现状**：`src/core/agent_session.rs:2194` 的 `RETRYABLE` 列表与 pi 0.99.1 的列表逐条对齐
（`overloaded` / `currently experiencing high demand` / …），但**没有** `"model is at capacity"`。

**建议**：在 `RETRYABLE` 中加入 `"model is at capacity"`，并在
`retryable_error_classification_matches_pi` 测试里补断言。

> **状态：已实现（第七节）**。`RETRYABLE` 已含 `"model is at capacity"`（`agent_session.rs:2249`）。
> 测试：`retryable_error_classification_matches_pi`。

---

### 1.3 🔧 未识别 z.ai CN 端点的 `"Prompt exceeds max length"` 上下文溢出

**pi 0.99.2 Fixed**（ai #10208）：`OVERFLOW_PATTERNS` 增加
`/prompt exceeds max length/i`。

**prux 现状**：`src/core/agent_session.rs:2956` 的 `OVERFLOW` 子串表含 `prompt is too long`、
`prompt too long`、`context_length` 等，但 `"Prompt exceeds max length"` 一个都不命中。

**建议**：加入 `"prompt exceeds max length"`，并补溢出恢复测试。

> **状态：已实现（第七节）**。`OVERFLOW` 已含 `"prompt exceeds max length"`（`agent_session.rs:3011`）。
> 测试：`overflow_error_patterns_cover_providers`。

---

### 1.4 🔧 NVIDIA 默认模型指向已被上游停服的模型

**pi 1.0.1 Fixed**：`nvidia/nemotron-3-super-120b-a12b` 已不再由 NVIDIA 提供，默认改为
`nvidia/nemotron-3-ultra-550b-a55b`。

**prux 现状**：`src/core/model_resolver.rs:1181` 仍为
`"nvidia" => "nvidia/nemotron-3-super-120b-a12b"`；目录里两个模型都还在
（`assets/models/models.all.json:9314` super、`:9345` ultra）。

**建议**：改默认模型表；并按第四节评估是否把停服的 super 从目录清理（pi 侧目录保留，先对齐默认值即可）。

> **状态：已实现（第七节 + 第十节）**。默认模型表改为 `nvidia/nemotron-3-ultra-550b-a55b`
> （`model_resolver.rs` 的 `default_model_id`），测试 `default_model_follows_pi_table`；
> 目录在第十节按 pi-ai **1.0.2** 全量重同步，`super` 与 pi 一样仍留在目录里。

---

### 1.5 🔧 Sign in with ChatGPT 回调端口被占用时静默退化为手动粘贴

**pi 1.0.1 Fixed**（ai #10265）：回调服务器绑定失败（`EADDRINUSE`）时不再继续流程，
而是明确报错 “Port 1455 is in use, probably by an unfinished login in another pi session
or by the Codex CLI.”；旧行为会让浏览器回调落到别处并显示 “OAuth state mismatch”。

**prux 现状**：`src/core/oauth/openai_chatgpt.rs:74`

```rust
let server = CallbackServer::bind_with(CALLBACK_PORT, CALLBACK_PATH, true).ok();
```

绑定失败即 `None`，流程照常给出授权 URL 并等待用户粘贴 redirect URL（注释也写明
“端口被占用（Codex CLI 等）… 只能粘贴”）。

**建议**：区分 `EADDRINUSE` 与其他错误，前者直接返回错误提示（中文文案），不再进入粘贴回退。
注意 pi 同时移除了 `Promise.race` 对 `callback` 为 undefined 的分支，prux 对应清理
`LoginKind::AuthorizeCode { server: None }` 在 OpenAI 路径上的使用。

> **状态：已实现（第七节）**。`CallbackServer::bind_raw` 区分 `AddrInUse` 与其他错误，**端口被占用
> 直接报错**，不再静默退化到粘贴 redirect URL（原粘贴回退已从 OpenAI 路径移除）。
> 测试：`occupied_callback_port_fails_instead_of_degrading`。

---

### 1.6 🔧 折叠的 codemode / MCP 结果按「逻辑行」截断，超长单行撑满屏幕

**pi 0.99.2 Fixed**：折叠预览像 bash 一样按**视觉行**（折行后）限制，而不是逻辑行；
minified JSON 这类单行输出不再占满屏幕。

**prux 现状**：`src/modes/interactive/render/messages.rs:1376` 起，只有 bash 走视觉行；
非 bash（含 `mcp`）是

```rust
let head = &out_lines[..out_lines.len().min(preview_lines)];
let preview = head.iter().flat_map(|l| wrap_text_lines(l, inner)).collect();
```

先按逻辑行取前 N 行，再逐行折行并**全部**收集 → 单条超长逻辑行会折出整屏。这正是 pi 修掉的问题。

**建议**：非 bash 分支也先折行再按视觉行截断（对齐 `truncateToVisualLines` 的 `keep: "start"` 语义），
提示行仍放在末尾。

> **状态：已实现（第七节）**。非 bash（含 `mcp`）分支改为**先折行、再按视觉行截断**，提示行仍在末尾。
> 测试：`non_bash_fold_limits_visual_lines_for_long_single_line`。

---

### 1.7 🔧 系统主题未限制 OKLCH chroma，pastel 调色板被拉得过艳

**pi 1.0.0 Fixed**（coding-agent #10255 / PR #10293）：Catppuccin Frappe 这类 pastel 调色板
在换算到别的明度时会「涨 chroma」，现在以源色的 OKLCH chroma 设上限并沿用同一 falloff。

**prux 现状**：`src/modes/interactive/system_theme.rs:1190` 的 `anchored()` 只按 OKHSL 饱和度缩放：

```rust
okhsl_to_rgb(source.h, source.s * falloff * saturation, lightness)
```

没有 chroma 上限，因此同样的饱和度在别的明度下可以对应更大 chroma → 与 pi 修复前同病
（prux 已在第二十轮移植了系统主题，但只覆盖到 pi 0.99.1 的形状）。

**建议**：给 `Okhsl` 源色补 `chroma`（`rgb_to_oklch(...).c`），`anchored()` 里
`c <= source.chroma * falloff * saturation` 时保留，否则用 `oklch_to_rgb(l, cap, hue)` 重建；
补 Catppuccin Frappe 粉色的回归断言。

> **状态：已实现（第七节）**。`anchored()` 以源色的 OKLCH chroma 设上限（`source.chroma * falloff * saturation`），
> 超出时用 `oklch_to_rgb` 重建。参考值随 pi 1.0.0 更新（`syntaxVariable` `#25b0d5`→`#36afd2`、
> `success` `#12bd7a`→`#15bd7a`）。测试：`pastel_palette_keeps_chroma_at_other_lightnesses`。

---

### 1.8 🔧 输入以空白开头时 slash 补全不触发

**pi 1.0.0 Fixed**（tui #10218）：命令补全对 `textBeforeCursor` 先 `trimStart()` 再判断 `/`。

**prux 现状**：`src/modes/interactive/handlers/suggest.rs:350` 的
`refresh_command_suggestions()` 直接 `prefix.strip_prefix('/')`，`prefix` 是光标前整段文本
（含前导空白），因此 `" /model"` 不触发候选。子命令分支 `refresh_subcommand_suggestions()`
同样以 `strip_prefix('/')` 起步。

**建议**：对命令/子命令分支的 prefix 做 `trim_start()`（注意保留 `prefix` 用于替换时的偏移，
pi 的做法是把 `commandText` 作为新 prefix 返回）。

> **状态：已实现（第七节）**。命令与子命令两个分支都对前缀做 `trim_start()`，`" /model"` 能触发候选。
> 测试：`slash_suggestions_allow_leading_whitespace`。

---

### 1.9 ⚠️ `codemode.mode: "only"` 下系统提示仍列 read/bash/edit/write

**pi 0.99.2 Fixed**（coding-agent #10192）：mode 为 `only` 时请求只声明 `codemode`，
但系统提示的 Available tools 仍列出四个内置工具；已修。

**prux 现状**：`src/core/system_prompt.rs:146`，`selected_tools` 为空时回落
`read/bash/edit/write`。mode 的消费者目前只见 `src/extensions/codemode.rs:144`
（`build_loadout`，影响 codemode 自己的描述/隐藏声明），未看到它同步影响
`SystemPromptOptions.selected_tools`。

**建议**：确认 `Mode::Only` 时 `selected_tools` 是否已被清空；若被清空，就必须让
`build_default_system_prompt` 在「显式空」与「未提供」之间有区别，否则永远命中错误回落。
补齐后可加一条「mode=only 时 Available tools 只含 codemode」的断言。

> **状态：已实现（第八节）**。`SystemPromptOptions.selected_tools` 改为 `Option<Vec<String>>`
> 区分「未指定」与「显式为空」，`compose_tools` 改为在 `apply_prepare_loadout` 之后用
> `selected - hidden` 建系统提示。

---

### 1.10 ⚠️ `--models` 尾随逗号多出一个模型

**pi 1.0.1 Fixed**（coding-agent #10334）：`--models` 分割后 `.filter(pattern => pattern.length > 0)`。

**prux 现状**：`src/cli/args.rs:68` 用 clap 的 `value_delimiter = ','`，clap 默认会为
`"a,"` 产生一个空字符串元素 → 可能多出一个空 pattern 进入 Ctrl+P 循环。

**建议**：核对 `parses_session_id_and_models` 之外的尾随逗号用例；需要过滤空串。

> **状态：已实现（第八节）**。`Args::normalize()` 丢弃空 pattern，两个解析入口
> （`parse_with_extension_flags` / `parse_from_args`）都会走。

---

### 1.11 ⚠️ 新会话偶发忽略已保存默认模型（扩展注册的原生 provider + 已存凭据）

**pi 0.99.2 Fixed**（coding-agent #9962 / PR #10190）：默认模型属于「扩展注册的原生 provider」
且该 provider 已存凭据时，新会话会忽略保存的默认模型，或提示没有可用模型。

**prux 现状**：prux 第十八轮已支持扩展注册原生 provider（`src/core/extensions.rs:420`
`image_apis`、provider 注册表）与 `core/model_resolver.rs` 的目录叠加。是否复现需要实测，
重点是「按凭据判定 provider 可用性」的路径（`src/core/auth.rs` `list_configured_providers`
与 `resolve_provider_model`）。

**建议**：写一条集成测试：临时 agent_dir + 扩展注册 provider + 存凭据 + 保存默认模型 →
新会话应解析出该模型。

> **状态：已核实无此问题（第十节）**。`virtual_models.rs` 的
> `saved_default_model_resolves_for_extension_registered_provider` 按上述场景断言：
> `resolve_provider_model` 拿到保存的扩展 provider 模型、`find_model` 命中虚拟条目
> （`api` 为虚拟模型 api）、`list_configured_providers` 也包含该 provider（凭据在 auth.json）。
> 即 pi 的「按凭据判定可用性」缺口在 prux 不存在；测试作为回归护栏留下。

---

### 1.12 ⚠️ 两处性能退化：提交 prompt 变慢、刷新后的远程目录合并平方复杂度

**pi 0.99.2 Fixed**（coding-agent #10198 等）：
- 解析会话的模型选择时每条 assistant 消息都查一次模型目录 → 提交随会话长度变慢；
- 合并远程（pi.dev）目录模型用了平方复杂度。

**prux 现状**：目录合并见 `src/core/model_resolver.rs:342`（`thinkingLevelMap` / `samplingParams`
逐键合并）与 `src/core/model_refresh.rs`；未测过平方行为。

**建议**：低优先，但值得写一个「远程目录 N 条 → 合并耗时线性」的基准或断言，避免同类退化。

> **状态：已处理（第十节）**。
> - 前半（每条 assistant 消息查一次模型目录）：prux 的会话模型选择只走
>   `virtual_models::previous_route`（从后往前一遍 `agent.messages` 读消息字段，不访目录），
>   不存在按消息数次查目录的路径，无此退化；
> - 后半（远程目录合并平方）：`model_resolver::provider_models` 的动态目录 upsert 原本每条
>   `position()` 扫全表（O(n·m)），已改为 `HashMap` 目录键索引（O(n+m)）；
>   断言见 `dynamic_catalog_merge_upserts_by_key_without_scanning`（500 条动态目录 + 覆盖/追加/
>   未知 type 丢弃）。

---

### 1.13 🔧 codemode `image()` 只校验 data URI 形式，不校验 base64 有效性与图片签名

**pi 0.99.2 Fixed**（codemode #10215）：`image()` 现在要求「合法 base64 的 PNG/JPEG/GIF/WebP」，
**按图片签名推导 MIME**（而非采信声明的 type），并会剥掉换行包裹的 base64；
否则抛 `TypeError`。旧行为会持久化非法 image block，导致之后每次 provider 请求 400。

**prux 现状**：`src/extensions/codemode/glue.js:255` 的 `image()` 只检查
`data:` scheme、`;base64` 标记、非空 data，并且直接使用 header 里的声明 MIME 传给
`__output("image", data, mime)`；沙箱侧 `src/extensions/codemode/sandbox.rs:405` 原样打包成
`ScriptOutput::Image`。没有 base64 解码、签名嗅探、换行清理、MIME 白名单。

**建议**：在宿主侧（Rust）或 glue 里补：base64 解码校验、PNG/JPEG/GIF/WebP 签名嗅探、
MIME 白名单、去除 `\r\n`，失败抛 `TypeError`。可复用 `src/utils/image.rs` 里的 mime 常量
`INLINE_IMAGE_MIMES`。

> **状态：已实现（第七节）**。glue 侧新增 `IMAGE_SIGNATURES`：要求合法 base64、
> 去除换行包裹的 base64、**按签名推导 MIME**（不采信声明的 type），失败抛 `TypeError`。
> 测试：`image_output_validates_base64_and_signature`。

---

### 1.14 🔧 codemode 脚本输出无上限，循环打印会撑爆宿主内存

**pi 1.0.1 Fixed**（codemode #10283）：脚本输出限制为 `MAX_OUTPUT_CHARS`（16 Mi 字符）
与 `MAX_OUTPUT_ITEMS`（100000 项），任一超限脚本以 `RangeError` 失败。

**prux 现状**：`src/extensions/codemode/sandbox.rs:405` 的 `__output` 无任何累计上限，
一直往 `Arc<Mutex<Vec<ScriptOutput>>>` 里追加。

**建议**：在 `__output` 里累计字符数与条目数，超限即通过既有的失败通道
（`done(false, ...)` / 抛错）让脚本以 `RangeError` 语义终止；常量对齐 pi 的 16 Mi / 100000。

> **状态：已实现（第七节）**。`glue.js` 的 `output()` 累计字符数与条目数，超限以 `RangeError` 失败；
> 常量在 `sandbox.rs`（`MAX_OUTPUT_CHARS = 16 Mi`、`MAX_OUTPUT_ITEMS = 100_000`）。
> 测试：`output_limit_is_enforced`。

---

### 1.15 ✅ OpenAI Responses 回放 grammar 工具调用时的 item id 前缀

**pi 1.0.0 Fixed**（ai）：不同 provider / 网关回放 `custom_tool_call` 时，只保留与「本次回放类型」
匹配的 id 前缀（`function_call` 必须 `fc_`、`custom_tool_call` 必须 `ctc_`），不同模型一律丢弃。

**prux 现状**：`src/core/provider/responses.rs:314` 已实现等价语义：

```rust
let item_id = if is_same_model && allowed {
    let prefix = if grammar_property.is_some() { "ctc_" } else { "fc_" };
    item_id_raw.filter(|raw| raw.starts_with(prefix))
} else { None };
```

（`allowed` 见 `:41` 的 `ALLOWED_TOOL_CALL_PROVIDERS`）。与 pi 修复后的条件一致。
**结论**：无需改动；建议保持注释与 pi 的 “a call can switch between the two types when grammar tool
support differs” 对应即可。

---

### 1.16 ✅ `--provider` 不带 `--model` 不会被静默忽略

**pi 1.0.1 Fixed**（coding-agent #10236）：旧行为会忽略 `--provider` 并用别的 provider 的默认模型，
现改为报错。

**prux 现状**：`src/core/model_resolver.rs:1085` `resolve_provider_model` 的
`(Some(p), None)` 分支在未给模型时使用**该 provider 的默认模型**（`default_model_for(&p)`），
本就不会「跑另一个 provider 的默认模型」。**结论**：prux 无此 bug，无需照搬 pi 的报错行为。

---

### 1.17 ❌ Anthropic strict tool use 拒绝 `minimum`/`maximum` 的 schema

**pi 0.99.2 Fixed**（ai #9953）：`strict: "prefer"` 的 schema 含 Anthropic strict 不接受的
关键字时改以非 strict 发送。

**prux 现状**：Anthropic 协议实现只对部分工具透传 schema（`src/core/provider/anthropic.rs`，
测试桩里 `supports_strict_mode: false`），没有 `strict: "prefer"` 这条路径。
**结论**：结构性不适用。

---

### 1.18 ❌ Amazon Bedrock 定价档 / stale thinking block

**pi 1.0.1 Fixed**（ai #10326 定价、#10324 thinking block）：prux 未实现 `amazon-bedrock`
provider（`src/core/login_registry.rs:8` 明确排除 bedrock）。**结论**：不适用。

---

### 1.19 ❌ Cloudflare AI Gateway 的 dashed model id（`claude-opus-5-5` vs `claude-opus-5.5`）

**pi 1.0.1 Fixed**：prux 只把 `cloudflare-ai-gateway` 当 base URL 网关处理
（`src/core/model_resolver.rs:542`），没有该 provider 的独立模型表。**结论**：不适用。

---

### 1.20 ⚠️ Together DeepSeek V4 Pro 改名后丢失 thinking level 控制

**pi 1.0.1 Fixed**（ai #10336）：Together 把模型改名为 `deepseek-ai/DeepSeek-V4-Pro-0813`
后 thinking level 控制失效。

**prux 现状**：目录里 `deepseek-ai/DeepSeek-V4-Pro` 与 `deepseek-ai/DeepSeek-V4-Pro-0813`
都在（`assets/models/models.all.json:887/927` 等）。需要确认 `-0813` 条目的
`thinkingLevelMap` / `reasoning` 是否与旧条目一致。

**建议**：按 pi 目录对照一次 together 的这两个条目字段。

> **状态：已修复（第十节）**。目录已按 pi-ai **1.0.2** 全量重新同步（`scripts/sync-models.py`，
> 见第四节）：`deepseek-ai/DeepSeek-V4-Pro-0813` 现在是
> `thinkingLevelMap.high = "high"` + `compat.supportsReasoningEffort = true`，与修复后的 pi 一致。

---

### 1.21 ❌ MCP `StreamableHttpTransport` 在 Cloudflare Workers 上的 `Illegal invocation`

**pi 0.99.2 Fixed**（mcp #10188）：JS `fetch` 调用不可带 receiver。prux 用 Rust `rmcp`
的 streamable-http 传输（`Cargo.toml:68`），无此运行时。**结论**：不适用。

---

### 1.22 ⚠️ 用户消息渲染保留两份全宽副本

**pi 1.0.0 Fixed**（coding-agent）：不再用 `Box` 包 `Markdown`，改由 `Markdown` 自带
padding/背景，避免每条渲染行有两份。prux 是 ratatui + `MessageCache`
（`src/modes/interactive/render/messages.rs:2256` 起），架构不同，**大概率无此问题**。
**结论**：核对用户消息分支没有重复渲染即可，预期无需改动。

---

### 1.23 ⚠️ Markdown 解析结果弱引用 + 缓存行扁平化（长消息内存）

**pi 1.0.0 Fixed**（tui）：`Markdown` 以 `WeakRef` 持有 token，`Markdown`/`Text`/`Box`
`flattenLines()` 缓存行；长 assistant 消息堆占用降到约 1/5。

**prux 现状**：prux 用**全局有界**解析缓存（`src/modes/interactive/render/markdown.rs:96`
`PARSE_CACHE_ENTRIES = 64`、`:98` `PARSE_CACHE_BYTES = 4 MiB`），并在第十七轮做了
「跨主题/宽度复用」；每条消息另有 `MessageCache` 缓存渲染行（`Arc<Vec<Line>>`）。

**结论**：问题形态不同（prux 不存在「每条消息各持一棵 token 树」），无需照搬；
若要优化内存，方向是评估 `MessageCache` 是否对不可见历史消息留了过多行。

---

### 1.24 ⚠️ MCP 工具调用在恢复会话 / HTML 导出中一直完全展开

**pi 1.0.1 Fixed**（coding-agent #10285）：恢复的会话里 MCP 工具调用在服务器连上之前
一直全展开，或永久如此。

**prux 现状**：prux 的 MCP 是**单一 `mcp` 代理工具**（`src/extensions/mcp.rs:232`
`tools()` 返回一个 `mcp_tool(aggregate_exposure(&servers))`），没有「按 MCP 工具动态注册渲染器」
的结构，折叠逻辑在 `render/messages.rs` 里对 `name == "mcp"` 已固定按 `MCP_PREVIEW_LINES` 折叠。
**结论**：结构性偏差，1.6 修好视觉行截断后此问题基本消失。

---

### 1.25 ⚠️ 扩展命令缺 `name` / handler 时按 `/` 崩溃

**pi 0.99.2 Fixed**（coding-agent #10054）：注册的扩展命令没有字符串 name 或 handler 时，
旧行为是输入 `/` 崩溃；现在扩展加载报错。

**prux 现状**：`ExtensionCommand`（`src/core/extensions.rs:172`）name 是 `String`，
天然排除非字符串；但**空字符串名**是否被拒绝需要核对（`register_slash_command` 与
`slash_commands()` 的拼装）。

**建议**：加载时校验空名/空 handler，失败即整扩展加载失败并给出可读错误。

> **状态：已修复（第十节）**。空名（以及含 `/`、空白的名）在声明层被丢弃并告警：
> `registry::registration_issues()`（原 `name_collisions`）新增非法名检查，
> `valid_name()` 同步过滤 `registered_commands()` / `command_provider()` / `command_busy_safe()`
> 与子命令列表，`/` 候选不再出现空条目。测试：`invalid_command_names_are_reported_and_ignored`。
> handler 在 Rust 侧是编译期函数指针，天然不可能缺。

---

### 1.26 ❌ 示例扩展 `built-in-tool-renderer.ts` / `minimal-mode.ts` 丢了内置工具摘要

**pi 0.99.2 Fixed**（coding-agent #10072）：这是 pi 的 **TS 示例扩展**。
prux 的 `examples/` 是 Rust SDK 示例（`examples/*.rs`、`examples/bot/`），没有对应示例。
**结论**：不适用。

---

### 1.27 ⚠️ `/reload` 与恢复会话时，已由 `tool_search` 加载的 deferred MCP 工具被丢弃

**pi 1.0.0 Fixed**：会话在 MCP 服务器重连前恢复了工具列表，导致 deferred 工具丢失。

**prux 现状**：deferred 机制见 `src/extensions/tool_search.rs`（`activate_deferred_tools` /
`clear_deferred_activations` / `take_deferred_activation_dirty`），MCP 是单一 `mcp` 工具、
不走 deferred 注册。**结论**：主要影响扩展注册的 deferred 工具；核对 `/reload`
（`src/modes/interactive/handlers/commands.rs:221` `reload_all_resources`）后
deferred 激活集是否被清空即可。

> **状态：已处理（第十节）**。两半都已核实：
> - `/reload`（`reload_from_settings`）**不**清空激活集，只有 `--no-extensions` 复位才清，
>   且 `dispatch_reload` 自己会 `rebuild_tools()`；
> - 恢复会话（启动 `--resume` / TUI fork/切换）会重建历史，而激活集是进程级内存状态——
>   这正是 pi 修的那一半。新增 `core::extensions::restore_deferred_activations(messages)`：
>   按恢复历史里的工具调用把**仍然声明为 deferred** 的工具重新激活，并在
>   `Agent::new` 与 `apply_fork_result` 里调用（后者随后 `rebuild_tools()`）。
>   测试：`restore_activations_from_restored_history`。

---

### 1.28 ⚠️ Anthropic 会话中途新增/重定义工具改为内联定义

**pi 1.0.0/1.0.1 Changed**（ai）：`tool_addition` 块内联工具定义（`inline-tools-2026-09-15` beta），
同名重定义不再回退成重发整份工具列表，保住 prompt cache；`hasToolRedefinitions()` 废弃。

**prux 现状**：prux **未实现** Anthropic 中途工具变更（无 `tool_addition`/`supportsMidConvoToolChanges`），
每轮发送当前完整工具列表，因此既没有该 bug，也没有该优化。**结论**：属「新增优化候选」，
若 prux 将来支持中途改工具再对齐；当前可跳过。

---

### 1.29 ⚠️ `/login` `/logout` 的状态与类型标签

**pi 1.0.0/1.0.1 Changed/Fixed**：无凭据由 “unconfigured” 改称 “not configured”；
只有订阅型 provider 标 “subscription”，其余 OAuth 标 “account”。

**prux 现状**：`src/modes/interactive/render/panel.rs:877` 注释与渲染仍是
`• unconfigured`；`src/core/login_registry.rs` 已有 `is_subscription` 字段（用于分组），
“subscription/account” 文案需核对。**结论**：低优先文案对齐。

> **状态：已对齐（第十节）**。`• unconfigured` → `• not configured`；
> `/logout` 列表在凭据类型混合时标 `[API key]` / `[subscription]` / `[account]`
> （`login_registry::is_subscription()` + `auth_type_label()`，可见性规则与 pi 的
> `showAuthTypeLabels` 同：同一种凭据类型不标）。`/login` 的 provider 列表按凭据类型过滤，
> 与 pi 一样不标类型标签。测试：`logout_panel_labels_mixed_credential_types`。

---

### 1.30 ⚠️ Apple Terminal 下的启动 header logo

**pi 1.0.0 Fixed**（coding-agent）：Apple Terminal 对半块字符渲染有缝，改用彩色文本 “Pi” 词标。

**prux 现状**：prux 的启动头（`src/extensions/banner.rs`、`render/startup.rs`）用的是自己的
banner 组件，没有 pi 那种半块 logo 组件，`src/utils/terminal_caps.rs` 也没有 Apple Terminal 探测。
**结论**：先确认 prux banner 是否有同型字符；若有再考虑 `TERM_PROGRAM=Apple_Terminal` 分支。

> **状态：已修复（第十节）**。prux banner 的 `████` art 是同型问题
> （块字符逐行留缝 → 横向断层）。新增 `utils::terminal_caps::is_apple_terminal()`
> （macOS + `TERM_PROGRAM=Apple_Terminal`，与 pi 同口径），命中时 `render_wordmark()`
> 改用纯文本词标布局（首行 accent 加粗词标 + dim 版本，其余信息行照旧，行数与 art 布局同形），
> 不再画块字符与竖线。测试：`apple_terminal_uses_text_wordmark`。

---

## 二、新增功能（0.99.1→1.0.2 新增，prux 没有 → 候选移植项）

### 2.1 🆕 `samplingParamsByThinkingLevel`（按 thinking level 覆盖采样参数）

**pi 1.0.2 New Features / Added**（coding-agent + ai #9776）：`models.json` 新字段，
按 `off/minimal/low/medium/high/xhigh/max` 给 OpenAI 兼容 API（completions、responses、
Azure responses）叠加 `temperature`/`top_p` 等；解析顺序
`model.samplingParams` → `samplingParamsByThinkingLevel[clamp(level)]` → 请求级 `samplingParams`。

**prux 现状**：只有 `samplingParams`：`src/core/model_resolver.rs:470` 字段、
`:760` 目录解析、`:850` 取 `temperature`、`src/core/provider/completions.rs:910` 全键合并。
无按 thinking level 的覆盖。**建议**：目录加 `samplingParamsByThinkingLevel` 字段，
在 `model_config_from_entry` 里按当前 thinking level 合并（提供者：completions/responses/azure）。

> **状态：已实现（第十一节）**。目录字段 + 按有效级别查覆盖项；completions / responses
> 两个 openai 兼容协议接线（prux 无独立 azure api，azure 端点走这两个协议）。

### 2.2 🆕 Anthropic workload identity federation（环境变量）

**pi 0.99.2 Added**（ai #10177 / PR #10242）：`ANTHROPIC_FEDERATION_RULE_ID`、
`ANTHROPIC_ORGANIZATION_ID`、`ANTHROPIC_IDENTITY_TOKEN_FILE`（可选
`ANTHROPIC_SERVICE_ACCOUNT_ID`、`ANTHROPIC_WORKSPACE_ID`）交给 Anthropic SDK 换取短期 token；
API key 与 `ANTHROPIC_AUTH_TOKEN` 优先。

**prux 现状**：`src/core/auth.rs:354` 只认 `ANTHROPIC_API_KEY` / `ANTHROPIC_AUTH_TOKEN` /
`ANTHROPIC_OAUTH_TOKEN`，没有 federation。**建议**：若 prux 的 anthropic 请求走自建 reqwest
（不走官方 SDK），需要自行实现 token 交换与刷新，成本较高；可先只做凭据来源识别与错误提示。

> **状态：第一步已实现、第二步明确不做（第十三节）**。`auth.rs` 新增 `ANTHROPIC_FEDERATION_REQUIRED` /
> `ANTHROPIC_FEDERATION_OPTIONAL` / `FederationConfig` / `anthropic_federation()` / `federation_note(provider)`：
> 三元组齐备时在「凭据缺失」的错误与启动诊断里追加明确说明（含 identity token 文件可读性），
> `/login` 的 provider 列表标 `• not configured · federation env (unsupported)`；
> 有 API key / OAuth 凭据时凭据优先、不提 federation。
> **token 交换明确不做**（12.1 第二步）：prux 没接 `@anthropic-ai/sdk`，交换端点与字段得靠抓包实测，
> 收益只覆盖 K8s 一类 federation 场景；现状是「配了三元组就明确告知本版本不支持」，不再排期。

### 2.3 🆕 Anthropic OAuth 显式「copy code login」

**pi 1.0.0/1.0.1 New Features/Added**（ai #10194）：登录时先选 Browser login（默认）或
Copy code login；后者用 `redirect_uri = https://platform.claude.com/oauth/code/callback`，
让用户在 Anthropic 页面复制 `code#state` 粘回 pi。

**prux 现状**：`src/core/oauth/anthropic.rs:65` 只绑定本地回调端口，端口占用时退化为
「粘贴 redirect URL」；没有显式方法选择，也没有备用 redirect_uri。
**建议**：在 `LoginKind`/登录面板加方法选择，新增 copy-code 分支（不同 redirect_uri + 无回调服务器）。

> **状态：已实现（第十三节）**。`LoginKind::AuthorizeCopyCode` + `anthropic::start_copy_code_flow()`：
> 不绑端口、`redirect_uri = https://platform.claude.com/oauth/code/callback`，
> token 交换回传同一个 `redirect_uri`（`exchange_login` 新增覆盖参数）；
> `/login` OAuth 路径下选 Anthropic 先出 `LoginMethod` 面板（Browser login / Copy code login），
> 面板文案与 `code#state` 输入行都已就位。粘贴解析沿用既有的
> `parse_authorization_input`（`code#state` / `code=..&state=..` / 整条 redirect URL / 裸 code）。

### 2.4 🆕 codemode `models.generateImages()`

**pi 1.0.0 New Features/Added**：脚本可用会话凭据跑图片模型，返回 base64 image block 由
`image()` 附加；用量计入会话成本；扩展侧对应 `ctx.modelRegistry.generateImages()`。

**prux 现状**：provider 层已有一次性 `generate_images`（`src/core/provider/images.rs`，
第十四轮落地），但 codemode 脚本全局里没有 `models.generateImages`：
`src/extensions/codemode.rs:637` 的全局分发只有 `searchTools`/`describeTool`/`models.*` 的
chat/classify 部分。**建议**：在 codemode 全局里接线到 `image_impl`，并把用量并入结果成本。

> **状态：已实现（第十一节）**。脚本全局接线到 `provider::generate_images`；用量并入
> codemode 结果，返回值形状与参数校验对齐 pi（`checkImagesContext` 口径），脚本没展示图片时补提示。

### 2.5 🆕 codemode `describeNamespace(name)`

**pi 0.99.2 Added**（coding-agent）：返回某命名空间的说明与工具名；`describeNamespace()` 与
`searchTools()` 接受 `mcp__dev-radius` / `mcp__dev_radius` / `dev-radius` / `dev_radius` 等形式。

**prux 现状**：有 `searchTools(query, {namespace})`（`src/extensions/codemode.rs:321`）与
`describeTool`（`:367`），**没有** `describeNamespace` 全局。**建议**：补一个全局即可
（数据在扩展工具的 namespace 字段里）。

> **状态：已实现（第八节）**。返回 `{ namespace, tools: [{ name, description }] }`；
> 未命中返回 `null`（宿主以 JSON `null` 表达「没有」，与 `describeTool` 实际行为一致）。
> 命名空间别名归一（`mcp__dev-radius` / `mcp__dev_radius` / `dev-radius` / `dev_radius`）
> 同时用于 `searchTools({namespace})`。

### 2.6 🆕 codemode：读取不存在的 `tools` / 命名空间成员应报错并给出近似名

**pi 1.0.0 Changed**（codemode）：`tools.Bash` 之类应抛错并提示 `tools.bash`，
需用 `"name" in tools` 探测；`models.classify()` / `models.generateImages()` 参数错误给出期望形状；
未知模型指向 `models.getAvailableOfType()`；超限 `store()` 说明用途；
生成图片却没展示时给提示。

**prux 现状**：`tools` 是 `Object.create(null)` + `Object.freeze` 的普通对象
（`src/extensions/codemode/glue.js:92`），不存在成员返回 `undefined`，没有近似名提示。
参数校验已有部分（`IMAGE_HELPER_EXPECTS`、`models.*` 类型错误文案、`store` 的 RangeError）。
**建议**：用 `Proxy` 包 `tools` 与命名空间对象，取不到成员时抛带 close matches 的错误；
`store()` 报错文案补“用途”说明。

> **状态：已实现（第十一节）**。`tools` 与全局命名空间都套 Proxy（近似名 + `Available:` 列表 + 提示），
> `store()` 超限文案补用途说明；`models.*` 的模型/上下文参数错误给出期望形状与 `getAvailableOfType` 指引。
> **注意行为变更**：读 `tools.<不存在>` 从「返回 `undefined`」改为**抛 `TypeError`**，
> 脚本探测成员需用 `"name" in tools`。

### 2.7 🆕 codemode 描述精简与错误恢复指引（约 40% prompt token）

**pi 1.0.0 New Features**：默认工具 + codemode 时 GPT-5.6 请求从约 5300 降到 3300 token；
声明里的工具说明压成一行，`models` API 指向 `docs/codemode.md` 由模型按需读取。

**prux 现状**：`src/extensions/codemode/description.rs` 已实现一轮压缩（含 `searchTools` 等），
是否达到 pi 1.0.0 的精简度需对照。**建议**：与 pi 1.0.2 的 `description.ts` 做逐段对照，
低优先。

> **状态：已实现（第十三节）**。`INTRO` 压到 6 行，`MODEL_API` 只留文档指引 + 五个一行签名，
> 类型定义搬进 `assets/docs/extensions/codemode.md`（新增「`models.*` 的类型定义」段）；
> 固定开销由 ~1600 token 降到 ~460（测试 `fixed_overhead_stays_small` 守住，
> 并断言类型块不得回描述里）。取舍见 12.3（已按「类型定义搬出描述」定案）。

### 2.8 🆕 MCP：项目级覆盖用户级服务器（无 `command`/`url` 的局部覆盖）

**pi 0.99.2 / 1.0.1 New Features/Added**（coding-agent #10277）：`.pi/mcp.json` 里一条
**只有** `enabled` / `exposure` / `toolExposure` 的条目，用来对一个用户级服务器做项目级覆盖；
`/mcp` 也能为当前项目启用/禁用。

**prux 现状**：已有 `prux mcp enable|disable --scope user|project`
（`src/cli/mcp_command.rs:14`、`:295`）与项目级配置（`.mcp.json`、`.prux/mcp.json`，
`src/extensions/mcp/config.rs:117` 起），`set_disabled`（`:615`）会用合并视图补最小条目。
但 `ServerEntry::validate()`（`config.rs:364`）**要求 command 或 url**，
所以「只有 exposure 的项目级局部覆盖」在配置文件形式下会校验失败。
**建议**：允许项目层条目缺 command/url，合并时只覆盖 `disabled`/`exposure`/`toolExposure`。

> **状态：已实现（第九节）**。合并语义本来就靠递归 `deep_merge` 支持局部覆盖，真正的坑在**写回**：
> `ServerEntry` 序列化会把未设字段写成 `null`，值级合并时把继承来的 `url`/`command` 擦掉，
> 于是「只有 exposure/enabled 的项目层条目」必然校验失败。已给空字段加 `skip_serializing_if`，
> `set_disabled` 改为只写最小覆盖条目（不再把用户层的 command/凭据抄进项目文件），
> `/mcp` 新增 `enable|disable <server> [user|project]`。

### 2.9 🆕 MCP `oauth.clientRegistration: "cimd"`（Client ID Metadata Documents）

**pi 1.0.1 New Features/Added**（mcp #10302）：以 pi.dev 上的 Client ID Metadata Document
标识 pi，而不是动态注册；配套 `OAuthClientProvider.clientMetadataDocument()`（破坏性 API 变更）
与 `extraPaths`/`path` 回调参数。

**prux 现状**：`rmcp 3.1.4` 已提供 `client_metadata_url`
（rmcp `transport/auth.rs:753`、`with_client_metadata_url`），但 prux 的 `OAuthConfig`
（`src/extensions/mcp/config.rs:265`）没有该开关。**建议**：加
`oauth.clientRegistration`（`"dynamic"` / `"cimd"`）+ `clientMetadataUrl` 之类的接线。

> **状态：已实现（第九节）**。配置项 `oauth.clientRegistration`（`dynamic` 默认 / `cimd`）+
> `oauth.clientMetadataUrl`，接线到 rmcp 的 `AuthorizationRequest::with_client_metadata_url`；
> 校验要求 cimd 时必须给出 https 且路径非根的 metadata 文档地址。
> 运行时路径已端到端验证（`examples/mcp_test_server` + `tests/mcp_oauth_e2e.rs`）。

### 2.10 🆕 MCP `oauth.authServerMetadataUrl`

**pi 1.0.0 Added**（mcp #10172）：服务器宣告了错误或缺失的授权服务器元数据时，用配置的
metadata 文档代替发现流程。

**prux 现状**：`OAuthConfig` 无此字段；`rmcp` 有 `is_allowed_authorization_server_metadata_url`
（rmcp `transport/auth.rs:1204`）但需确认是否暴露「显式指定 metadata URL」的入口。
**建议**：先确认 rmcp 能力，再决定是在 `OAuthConfig` 加字段还是自定义 discover 步骤。

> **状态：已实现（第九节）**，但**绕过了 rmcp 的 `start_authorization`**：rmcp 3.1.4 的
> `AuthorizationManager::start_authorization` 会无条件重新发现授权服务器元数据并覆盖
> `set_metadata` 设进去的值，`AuthorizationRequest` 也没有该字段，所以 prux 自己拉文档 →
> `set_metadata` → 直接建 `AuthorizationSession` 放进 `OAuthState::Session`。
> 已端到端验证「配置文档的授权端点被采用」；rmcp 升级后这段要重新核对。
>
> 顺带发现的 rmcp 行为：**发现流程对坏 metadata 很宽容**——文档缺 `token_endpoint` 时不报错，
> 而是回落到「按 base URL 合成端点」。所以 2.10 的测试改成断言「授权端点来自配置文档」，
> 而不是「没有配置时失败」（后者根本不会发生）。

### 2.11 🆕 MCP `auth: { provider: "<provider>" }`（用 provider 的登录 token 作 bearer）

**pi 0.99.2 Added**：HTTP MCP 服务器可用某 provider 的当前 `/login` token 作 bearer，
每次请求实时读取；仅允许全局 `mcp.json` 与扩展声明，非 loopback 需 https。

**prux 现状**：`AuthSetting`（`src/extensions/mcp/config.rs:234`）只支持 `"oauth"` / `"bearer"` /
`false`，没有 provider 形式。**建议**：加变体并在请求时从 `src/core/auth.rs` 取该 provider 当前 token。

> **状态：已实现（第九节 + 第十四节）**。`auth: {"provider": "<name>"}` → `AuthSetting::Provider`，
> 取 token 作 `Authorization` 头；`auth.provider` 只允许用户级配置或扩展声明（项目文件里出现即报错），
> 非 loopback 端点必须 https。**token 在每次请求前重新解析**（第十四节）：`LiveTokenClient` 包一层
> `StreamableHttpClient`，每个 `post_message` / `get_stream` / `delete_session` 都现取 token
> （`core::auth::resolve_provider_token`：OAuth 凭据优先，过期先刷新，否则 `auth.json` 的 key），
> 所以 `/login`、登出、凭据轮换都会在下一发请求生效，不需要 `/mcp reload`；
> 凭据缺失时请求直接报错并附 `/login <provider>` 指引，不退化成匿名请求。
> 已端到端验证（真实 401 → 带 token 连接 → 调用工具 → 轮换后下一发请求即用新 token）。

### 2.12 🆕 MCP 服务器 `description` 字段（含 `pi mcp add --description`）

**pi 0.99.2 Added**：`description` 随服务器出现在系统提示里，并用于 tool search 排序。

**prux 现状**：`ServerEntry`（`src/extensions/mcp/config.rs:318`）没有 `description`；
`prux mcp add` 也没有 `--description`（`src/cli/mcp_command.rs`）。
**建议**：加字段 + CLI 选项；prux 的 MCP 走单一 `mcp` 工具，可先用于 `/mcp` 展示与 search 排序。

> **状态：已实现（第九节）**。`ServerEntry.description` + `prux mcp add|update --description TEXT`；
> 摘要（`name — 说明`，单条截断 120 字符）拼进 `mcp` 工具的说明与 snippet（等价于 pi 的「随服务器进系统提示」），
> 并出现在 `/mcp status|list` 与 `prux mcp list`；由于摘要进了 `mcp` 工具描述，它也参与 tool search 的 BM25 文本。

### 2.13 🆕 `pi.registerToolRenderer()`：为未注册工具（如 MCP 工具）提供渲染器

**pi 1.0.1 New Features/Added**（coding-agent #10285）。

**prux 现状**：扩展 API（`src/core/extensions.rs`）有 `exposure`/`namespace`/`annotations`/
`output_schema`/`prepare_loadout`/`execute_tool`（第八、十四、十七轮），但**没有**渲染器回调；
工具渲染是宿主内置的（`src/modes/interactive/render/messages.rs` 按工具名分派）。
**建议**：评估是否加 `register_tool_renderer`，prux 是 Rust 同步渲染，接口形态需重新设计；
优先级中。

> **状态：已实现（第十三节）**。接口按 12.4 的提案落地但改为 prux 自有的样式类型（`core` 不依赖 ratatui）：
> `ToolRenderer { name / tools / render }`、`ToolRenderCtx`、`ToolRender { lines: Vec<RichLine>, preview_lines }`，
> `Extension::tool_renderers()` 声明 + `register_tool_renderer()` 直注册；
> 渲染路径在 `render_tool_block` 第一步分派（宿主的 `edit` diff 不参与接管），
> 宿主负责铺背景/折行/裁剪，`preview_lines` 按视觉行折叠；
> `MessageCache` 增渲染器指纹，`/reload` 或禁用扩展后旧行立即重画。

### 2.14 🆕 `/reload` 启用新加入 `defaultTools` 的工具

**pi 0.99.2 Added**（coding-agent #10245）：`/reload` 后，`defaultTools` 新增的工具被启用；
从中移除的工具保持启用；会话中被关掉的保持关闭（除非新加入）；`--tools`/`--no-tools`/
`--no-builtin-tools` 仍覆盖。

**prux 现状**：已有 `defaultTools` 的 `+name`/`-name` 语义（`src/core/settings_manager.rs:499`
起，`resolve_default_tools`），`/reload`（`handlers/commands.rs:221`）会重载
keybindings/extensions/skills/prompts/themes/context，但未见重读 `defaultTools` 并启用新工具。
**建议**：`reload_all_resources` 里重新应用 `read_settings_default_tools()`。

> **状态：已实现（第八节）**。`Agent::reload_default_tools()` 在 `dispatch_reload` 里调用，
> 只增不减；命令行显式指定过工具集时不重读。

### 2.15 🆕 `quietStartup: "header"`

**pi 1.0.0 Added**：只保留启动 header 的版本与按键提示，隐藏 model scope 与已加载资源清单。

**prux 现状**：没有 `quietStartup` 设置（grep 0）；有 `--verbose`。**建议**：加设置档位。

> **状态：已实现（第十一节）**。`quietStartup` 支持 `false` / `"header"` / `true`，`--verbose` 覆盖；
> **默认值口径与 pi 不同**：未设置时按 `"header"`（保持 prux 既有行为），见第十一节说明。

### 2.16 🆕 TUI 默认 fullscreen + `tuiMode`/`--tui-mode regular`

**pi 1.0.0 New Features/Changed**：默认全屏，`tuiMode: "regular"` 或 `--tui-mode regular`
保留终端原生 scrollback。

**prux 现状**：prux **一直**进入备用屏幕（`src/modes/interactive.rs:380`、
`:409` `EnterAlternateScreen`），没有 regular/inline 模式，也没有 `tuiMode` 设置或
`--tui-mode` 参数。**建议**：prux 已满足“默认 fullscreen”，缺的是 regular 逃生开关；
是否移植取决于是否有用户需要 scrollback。

> **状态：不做（第十、十一节已确认）**。pi 1.0.0 的「默认 fullscreen」prux 本来就满足（一直进备用屏幕），
> 缺的只是回滚到终端原生 scrollback 的 regular / inline 逃生开关；经确认不移植（无此需求）。

### 2.17 🆕 MCP 服务器默认不阻塞首个 prompt、`mcp_servers` 系统提示段

**pi 0.99.2 New Features/Changed**（coding-agent #10212）：默认 `codemode` exposure 的服务器
不再列进 codemode 描述，不再阻塞首个 prompt；改在系统的 `mcp_servers` 段落里一行摘要，
脚本用 `searchTools()`/`describeNamespace()` 找。

**prux 现状**：默认 exposure 是 `Direct`（已知刻意偏差，见 v0.99.1 指南 §20.5，
`src/extensions/mcp/config.rs:182`），codemode 描述本就不列 MCP 工具；
但没有 `mcp_servers` 系统提示段（`src/core/system_prompt.rs` 无 MCP 段），
连接是惰性的（`src/extensions/mcp/manager.rs:695` `ensure_connected`）。
**建议**：若采纳 2.5/2.12，可顺带加 `mcp_servers` 段落。

> **状态：已实现（第十一节）**。系统提示末尾新增 `<mcp_servers>` 段：只列工具不直接声明的
> 已启用服务器（`codemode` / `codemode-deferred` / `deferred`，含 `toolExposure` 命中），一行摘要 + 获取方式；
> 「不阻塞首个 prompt」prux 本就不阻塞（连接惰性），无需改动。段整体 4096 字符、单台摘要 250 字符上限。

### 2.18 🆕 OAuth 登录界面支持复制登录 URL 的按键

**pi 1.0.1 Added**：`app.message.copy`（默认 `ctrl+x`）在 `/login`、`/mcp`、`/mcp login`
的登录界面复制登录 URL。

**prux 现状**：`app.message.copy` 默认 `ctrl+x` 已存在（`src/core/keybindings.rs:601`），
但语义是“复制最后一条助手消息”（`src/modes/interactive/handlers.rs:435`），登录面板未接。
**建议**：登录面板接管该按键，复制授权 URL。

> **状态：已实现（第十一节）**。登录面板接管 `app.message.copy`，复制授权 URL；
> 提示行按真实键位显示「ctrl+x to copy」（`keybindings::key_display`）；
> Ctrl+click 打开浏览器同样走 `handlers/mouse.rs` 的命中区域。

### 2.19 🆕 `/mcp login` 的登录 URL 折行后可点击（OSC8 hyperlink）

**pi 0.99.2 / 1.0.0 Fixed**（coding-agent #10186）。

**prux 现状**：需核对 `src/modes/interactive/render/panel.rs` 里 OAuth/mcp 登录 URL 的渲染
是否发 OSC8 超链接、折行时是否分段。**建议**：低优先。

> **状态：已实现（第十一节 + 第十四节）**。OSC8 无法穿过 ratatui 的 **text 渲染通道**（`set_stringn`
> 过滤含控制字符的 grapheme），但 ratatui 0.30 的 `Cell::set_symbol` + `CellDiffOption::ForcedWidth`
> 可以承载转义序列（官方为超链接/kitty 图片预留的路），所以改为在 **buffer 定型后**改 cell symbol：
> 新增 `render/hyperlink.rs`（`LINK_START` / `LINK_END` 哨兵 + `apply`），
> 登录面板的授权 URL 与 device-code 验证 URI 每个折行段各发一组 OSC8 序列，
> `render_frame` 末尾统一翻译（终端不支持 OSC8 时只剥哨兵，退化为纯文本）。
> Ctrl+click 命中区域保留（不支持 OSC8 的终端靠它）。
> 消息区的 markdown 链接一开始没接（点了没反应），第十五节补齐：`[文本](url)` / `<url>` / 邮箱
> 既发 OSC8（终端原生点击），也记成屏幕命中区（`ctrl+click` 打开），两条路都以同一份
> `app.mouse.links` 为准。
> **顺带发现的既有 bug 已修（第十三节）**：`render/markdown.rs` 的 OSC8 分支已删除
> （以前会打印 `]8;id=none;…` 字面量），链接现在只输出可见文本 + ` (url)`。

### 2.20 ✅ MCP OAuth 协议层加固（iss / step-up / 空字段 / expires_in / 分页）

**pi 0.99.2–1.0.1 Fixed**：RFC 9207 `iss` 校验、`insufficient_scope` step-up 保留已授 scope、
空/`null` 可选字段不报 `Invalid scope`、`expires_in: null` 不视为已过期、保护资源元数据 URL
非法时回落 origin、空 scope 不覆盖下一来源、分页 `nextCursor: ""`/`null` 结束、按 server 名+URL
分别存凭据。

**prux 现状**：这些协议细节由 `rmcp 3.1.4` 实现（`Cargo.toml:68` 启用 `auth` feature），
源码里可见 `iss`（`transport/auth.rs:2014`、`:2038`）、`insufficient_scope`（`:656`、`:2292`）、
`scopes_supported`（`:585`、`:1926`）、`expires_in`（`:2160`）、`client_metadata_url`（`:753`）；
凭据按 server 名 + URL 隔离（`src/extensions/mcp/oauth.rs:773` 测试）。
**结论**：已由 rmcp 覆盖；升级时只需确认 rmcp 版本，不必在 prux 重写。配置面缺口（2.9 / 2.10）
已在第九节补上。

### 2.21 ✅ `oauth.clientName`

**pi 0.99.2 Added**。prux 已有 `client_name`（`src/extensions/mcp/config.rs:279`），
动态注册时使用（`src/extensions/mcp/oauth.rs:554`）。**结论**：已满足。

### 2.22 ✅ `codemode-deferred` 作为 `codemode` 别名

**pi 0.99.2 Changed**。prux 的 `McpExposure::CodemodeDeferred` 与 `Codemode` 行为等同
（`src/extensions/mcp/config.rs:188` 注释）。**结论**：已满足。

### 2.23 ❌ Cloudflare Clef / Clef Flash 分类器

**pi 1.0.1 Added**（ai #10316/#10322，可在 codemode 与扩展里用）。prux 未实现
`cloudflare-workers-ai` provider。**结论**：不适用（除非将来加该 provider）。

### 2.24 ❌ Radius 登录与 Radius MCP 一键配置

**pi 1.0.0 New Features**。prux 明确不做（`src/core/login_registry.rs:10`
“radius 为网关 provider（动态模型目录），暂不实现”）。**结论**：刻意不做。

### 2.25 🔧 `setImageTranscoder()` / Kitty 非 PNG 图片 / 全屏图片滚动

**pi 1.0.1 Added/Fixed**（tui #10292、#10319）与 **1.0.0** 的
`TuiAltScreen.getScreenLines()`。prux 原不做终端内联图片渲染（只显示 `[image]` 占位，
`render_image_placeholder`）。**结论**：结构性不适用——**该结论已推翻，见第十六节**。

> **状态：已实现（第十六节，2026 重评）**。改用 `ratatui-image 11.1` 承载终端图形协议
> （kitty 的 unicode placeholder / iTerm2 / sixel / halfblocks），由 settings.json
> `showImages`（**默认关闭**，关闭时与既有 `[image]` 占位逐字节一致）门控；
> 消息附件、assistant 内容块、工具结果（read 截图）三处都能内联显示。
> `setImageTranscoder()` 本身不适用（那是给 JS 扩展注入 PNG 转码器的钩子，
> prux 有 `image` crate，转码是内部实现细节），但「Kitty 只吃 PNG」这条由库内部处理；
> 「全屏图片滚动」由库的 `SlicedImage` + 行偏移裁剪解决（滚动到半出视口只画可见部分）。

### 2.26 ❌ Nix flake

**pi 1.0.1 New Features**（#9137）。prux 是 Rust 二进制，发布/分发形态不同。
**结论**：不适用（如需可另开 Rust 侧的打包工作）。

---

## 三、移除 / 破坏性变更

### 3.1 ❌ `packages/agent`：移除 experimental harness

**pi 1.0.0 Breaking**（agent）：从 `@earendil-works/pi-agent-core` 移除
`AgentHarness`、sessions/session storage、durable runtime、pico3、harness tools、compaction、
skills、prompt templates、system prompt helpers、telemetry schemas、search service types、
`uuidv7`/pi-telemetry re-export；子路径导出 `./node`、`./harness/*`、`./experimental/pico3` 消失；
包只剩 `Agent` + agent loop + proxy stream。durable 迁到新包 `@earendil-works/pi-durable`。

**prux 现状**：prux 是单 crate，agent loop / 会话 / compaction / skills / prompt templates /
system prompt 全在 `src/core/`（`agent_loop.rs`、`agent_session.rs`、`compaction/`、`skills.rs`、
`prompt_templates.rs`、`system_prompt.rs`、`session_v4.rs`），没有 JS 包的子路径导出概念。
**结论**：结构性不适用，无需改动。

### 3.2 ❌ `packages/server`：`SessionMetadata` 改由本包导出

**pi 1.0.0 Breaking**（server）。prux 没有 server 包。**结论**：不适用。

### 3.3 ❌ 发布/打包类移除

- `npm-shrinkwrap.json` 从发布包移除（1.0.1，coding-agent #5653）；
- `pi update` 对全局 npm 安装推荐迁到 pi.dev 托管安装；
- `pi-ai/models` 轻量入口（0.99.2 Added，ai）；
- `CodemodeSandbox.workerUrl` 允许字符串以支持 Bun 编译产物（0.99.2）。

均为 JS/Node 打包与运行时；prux 用 Cargo 发布。**结论**：不适用。

### 3.4 ⚠️ `packages/durable` 新包与 provider session UUID

**pi 1.0.0 Added**（durable 初版）、**1.0.2 Fixed**（#10424）：每个对话持久化一个独立的
provider session UUID 并随请求下发，用于 prompt cache 与 session 亲和。

**prux 现状**：prux 没有 durable 架构（v0.99.1 指南已判结构性不适用）；但**已有**会话亲和：
`src/core/model_resolver.rs:840` 的 `session_id` 用于 anthropic `x-session-affinity` /
opencode `x-opencode-session`（`src/core/provider/anthropic.rs:1461`、`completions.rs:953`）。
需要确认该 id 是否**按对话持久化**（而非每次启动重算），以及是否覆盖所有需要的 provider。

> **状态：已核实并补齐（第十节）**。
> - 持久化：`session_id` 在会话创建时生成并写进会话文件头（`session_manager`），
>   `Agent::new` 从 `session.session_id` 取用 → 同一对话跨重启稳定，不是每次启动重算；
> - 覆盖：原先只有 anthropic 与 completions 无条件发 `x-session-affinity`。已按 pi 1.0.2 的
>   规则重写为 `provider::session_affinity_headers()`：`compat.sendSessionAffinityHeaders`
>   （缺省 = 是否 openrouter）门控 anthropic-messages / openai-completions，
>   openai-responses 只要有会话 id 就发；格式 `openrouter` → `x-session-id`，
>   `openai` → `session_id` + `x-client-request-id`（completions 再补 `x-session-affinity`），
>   `openai-nosession`（opencode 目录）→ 不带 `session_id`；anthropic 只用 `x-session-affinity`。
>   opencode 的 `x-opencode-session`/`x-opencode-client` 仍按 provider 判定（与 pi 的
>   provider attribution 一致），已在 `provider_extra_headers` 里对所有协议生效。
>   代理网关在客户端自带会话头时强制开启门控（否则客户端给的 id 到不了上游）。
>   测试：`session_affinity_headers_follow_api_and_compat`。

---

## 四、模型目录同步项（升级时的机械性工作）

1. **nvidia 默认模型**：`nvidia/nemotron-3-super-120b-a12b` → `nvidia/nemotron-3-ultra-550b-a55b`
   （见 1.4）。
2. **together**：核对 `deepseek-ai/DeepSeek-V4-Pro-0813` 的 reasoning/thinkingLevelMap
   （见 1.20）。
3. **cloudflare-workers-ai**：若要加 Clef 分类器模型（2.23），需先加 provider。
4. 其余 provider 目录条目在 0.99.1→1.0.2 区间无结构性变化（GPT-6.1 Sol 已在 0.99.1 落地，
   见 `migrations/migration-v0.99.1.md` §1.2），按 pi 目录做一次全量 diff 即可。

> **状态：已完成（第十节）**。用本机 pi 1.0.2 的 `@earendil-works/pi-ai` 数据跑了
> `scripts/sync-models.py`：34 个 provider、22 个文件更新（+2845/−1291），
> `--check` 现为 0 差异。相对 0.99.1 基线的新增/变更 provider：baseten、github-copilot、
> nvidia、opencode（`sessionAffinityFormat: "openai-nosession"`）、openrouter（±12/−2）、
> together（见 1.20）、vercel-ai-gateway、cerebras / fireworks / openai 少量字段。

---

## 五、剩余工作与优先级

| 状态 | 项 | 说明 |
|---|---|---|
| **已落地** | 1.1–1.14（七节）；1.9 / 1.10 / 2.5 / 2.14（八节）；2.8–2.12（九节）；1.11 / 1.12 / 1.20 / 1.25 / 1.27 / 1.29 / 1.30 / 3.4 与目录全量同步（十节）；2.1 / 2.4 / 2.6 / 2.15 / 2.17 / 2.18（十一节）；2.2 第一步 / 2.3 / 2.7 / 2.13 + OSC8 字面量 bug（十三节）；2.11 实时读 token / 2.19 OSC8（十四节）；2.19 的消息区链接（十五节） | — |
| **不做（明确）** | 2.16、2.2 第二步 | 2.16：prux 本就默认全屏，缺的只是 regular 逃生开关；2.2 第二步：需实测 SDK 交换端点与字段（12.1），当前只给「已配置但不支持」提示 |
| **无需改动 / 不适用** | 1.15–1.19、1.21–1.24、1.26、1.28、2.20–2.24、2.26、第三节 | 结论见各条目末尾的 `> **状态：…**` |
| **本轮补做** | 2.25 终端内联图片（第十六节） | 曾判「结构性不适用」，重评后以 `showImages`（默认关闭）落地 |

---

## 六、验证命令

```bash
# 编译 + 全量测试（改动后必跑）
cargo test

# 本文涉及的重点测试
cargo test -p prux retry_after                            # 1.1（invalid_values_fall_back / http_date）
cargo test -p prux retryable_error_classification         # 1.2
cargo test -p prux overflow_error_patterns                # 1.3
cargo test -p prux default_model                          # 1.4 / 四.1
cargo test -p prux occupied_callback_port                 # 1.5
cargo test -p prux non_bash_fold_limits_visual_lines      # 1.6
cargo test -p prux pastel_palette_keeps_chroma            # 1.7
cargo test -p prux slash_suggestions_allow_leading        # 1.8
cargo test -p prux system_prompt_distinguishes_unset      # 1.9
cargo test -p prux models_drops_empty_patterns            # 1.10
cargo test -p prux image_output_validates_base64          # 1.13
cargo test -p prux output_limit_is_enforced               # 1.14

# 第八～十节新增/相关的重点测试
cargo test -p prux namespace_aliases_normalize            # 2.5
cargo test -p prux reload_default_tools                   # 2.14
cargo test -p prux invalid_command_names_are_reported     # 1.25
cargo test -p prux restore_activations_from_restored      # 1.27
cargo test -p prux logout_panel_labels_mixed              # 1.29
cargo test -p prux apple_terminal                         # 1.30
cargo test -p prux session_affinity_headers_follow        # 3.4
cargo test -p prux saved_default_model_resolves           # 1.11
cargo test -p prux dynamic_catalog_merge_upserts          # 1.12

# 第十一节新增/相关的重点测试
cargo test -p prux sampling_params                        # 2.1（四组断言）
cargo test -p prux script_generate_images                 # 2.4
cargo test -p prux missing_tool_member_names              # 2.6
cargo test -p prux quiet_startup                          # 2.15
cargo test -p prux mcp_servers_section                    # 2.17
cargo test -p prux login_panel_app_message_copy           # 2.18

# 第十三节新增/相关的重点测试
cargo test -p prux anthropic_federation_is_recognized   # 2.2（第一步）
cargo test -p prux copy_code                            # 2.3（后端 + 面板 + 选择步）
cargo test -p prux exchange_body_follows_redirect       # 2.3（token 交换回传的 redirect_uri）
cargo test -p prux fixed_overhead_stays_small           # 2.7（描述固定开销）
cargo test -p prux description_includes_model_api       # 2.7
cargo test -p prux renderer_follows_extension_activation # 2.13（渲染器注册表）
cargo test -p prux extension_renderer_takes_over        # 2.13（渲染路径接管与回退）
cargo test -p prux markdown_links_render_as_visible_text # OSC8 字面量 bug

# MCP 配置面 / 协议层（2.8–2.12 / 2.20–2.22）与目录一致性
cargo test -p prux mcp
cargo test --test mcp_oauth_e2e                            # 2.9 / 2.10 / 2.11 端到端（含 2.11 换 token 后立即生效）
cargo test -p prux hyperlink                              # 2.19（哨兵→OSC8 的翻译）
cargo test -p prux login_oauth_url_is_rendered_as_osc8    # 2.19（面板端到端）
cargo test -p prux message_links_become_clickable_rows    # 2.19（消息区链接：命中区 + 端到端）
python3 scripts/sync-models.py --check                     # 四（目录应与 pi-ai 1.0.2 零差异）
cargo run -- --list-models nvidia
```

---

## 七、实施记录（P0 落地，已完成）

| # | 项 | 改动位置 | 关键测试 |
|---|---|---|---|
| 1.1 | Retry-After 解析 | `src/core/provider/retry.rs`：`retry_after_ms` + 新增 `parse_retry_after` / `http_date_ms` | `retry_after_invalid_values_fall_back_to_backoff`、`retry_after_http_date` |
| 1.2 | at capacity 重试 | `src/core/agent_session.rs` `RETRYABLE` | `retryable_error_classification_matches_pi` |
| 1.3 | z.ai CN 溢出 | `src/core/agent_session.rs` `OVERFLOW` | `overflow_error_patterns_cover_providers` |
| 1.4 | NVIDIA 默认模型 | `src/core/model_resolver.rs` `default_model_id` | `default_model_follows_pi_table` |
| 1.5 | ChatGPT 端口占用 | `src/core/oauth/openai_chatgpt.rs` + `oauth.rs` 新增 `CallbackServer::bind_raw` | `occupied_callback_port_fails_instead_of_degrading` |
| 1.6 | 非 bash 折叠视觉行 | `src/modes/interactive/render/messages.rs::render_tool_output` | `non_bash_fold_limits_visual_lines_for_long_single_line` |
| 1.7 | 系统主题 chroma 上限 | `src/modes/interactive/system_theme.rs`：`SourceColor` + `anchored` | `pastel_palette_keeps_chroma_at_other_lightnesses` |
| 1.8 | slash 补全前导空白 | `src/modes/interactive/handlers/suggest.rs` | `slash_suggestions_allow_leading_whitespace` |
| 1.13 | codemode `image()` 校验 | `src/extensions/codemode/glue.js`：`IMAGE_SIGNATURES` + base64/签名校验 | `image_output_validates_base64_and_signature` |
| 1.14 | codemode 输出上限 | `src/extensions/codemode/glue.js` `output()` + `sandbox.rs` 两个常量 | `output_limit_is_enforced` |

- **参考值再生成**：1.7 的 `syntaxVariable`（`#25b0d5`→`#36afd2`）与 `success`（`#12bd7a`→`#15bd7a`）
  是 pi 1.0.0 加 chroma 上限后的真实输出，由 pi v1.0.2 的 `system-theme.ts` 在 Node 下实际运行导出（非手改）。
- **验证**：`cargo test` 全绿（lib 2252 项 + 集成测试）；`cargo fmt --check` 通过；
  `cargo clippy --tests` 仅剩改动前就存在的 3 条 warning（`settings_manager.rs`、`tasks/widget.rs`）。
- **未做（截至该轮）**：P1/P2 余项（2.1–2.4、2.6、2.7、2.13、2.15–2.19、1.22–1.24）当时尚未动代码；
  其中 1.9、1.10、2.5、2.14 见第八节，MCP 配置面 2.8–2.12 见第九节，
  1.11、1.12、1.20、1.25、1.27、1.29、1.30、3.4 与目录全量同步见第十节，
  2.1/2.4/2.6/2.15/2.17/2.18 见第十一节，**当前剩余项见零节与第十二节**。

---

## 八、实施记录（P1/P2 第一批，已完成）

小改动一批：一行级的 CLI 归一、系统提示的显式空语义、codemode 新全局、
`/reload` 重读 `defaultTools`。

| # | 项 | 改动位置 | 关键测试 |
|---|---|---|---|
| 1.10 | `--models` 空 pattern | `src/cli/args.rs`：新增 `Args::normalize()`，`parse_with_extension_flags` / `parse_from_args` 各调一次 | `models_drops_empty_patterns` |
| 1.9 | 显式空工具集 | `src/core/system_prompt.rs`：`selected_tools: Option<Vec<String>>` + `prompt_tool_names()`；`src/core/agent_session.rs::compose_tools` 改为在 `apply_prepare_loadout` 之后建提示，按 `selected - hidden` 传工具名 | `system_prompt_distinguishes_unset_from_explicit_empty_tools`、`codemode_only_mode_hides_direct_declarations` |
| 2.5 | `describeNamespace()` | `src/extensions/codemode.rs`：`namespace_key()` / `namespace_tools()` / `Host::describe_namespace`，注册进脚本全局；说明文案见 `codemode/description.rs` | `namespace_aliases_normalize`、`namespace_tools_lists_sorted_members`、`script_describe_namespace_lists_members` |
| 2.14 | `/reload` 重读 `defaultTools` | `src/core/agent_session.rs`：`settings_default_tools` 字段 + `set_settings_default_tools` + `reload_default_tools`；`src/modes/interactive/worker.rs::dispatch_reload` 调用；`src/main.rs` 记录启动基线 | `reload_default_tools_only_adds_new_names`、`reload_default_tools_skips_cli_override` |

- **行为口径**：
  - 1.9 的两处「空」不再混淆——`None`（外部调用方未指定）回落
    `DEFAULT_TOOL_NAMES`，`Some(vec![])`（`--no-tools`、`mode = only` 摘空后）渲染 `(none)`；
    副作用：提示里没有 `read` 时技能段不再渲染（`mode = only` 下与「模型直接调 read」的前提一致）。
  - 2.5 的未命中返回 `null` 而非 `undefined`：宿主以 JSON `null` 表达「没有」，与既有
    `describeTool` 的实际行为一致（其说明文案里的 `undefined` 是 pi 原文，prux 未做 `undefined` 通道）。
  - 2.14 只增不减：从 `defaultTools` 移除的工具保持启用（对齐 pi），会话里另行关掉的工具
    也不会因 reload 复活。
- **验证**：`cargo test` 全绿（lib 2258 项 + 集成测试）；`cargo fmt --check` 通过；
  `cargo clippy --tests` 仅剩改动前就存在的 3 条 warning。
  注：整包并行跑时 `extensions::subagent::schedule::tests::corrupt_store_is_quarantined_instead_of_silently_wiped`
  与 `modes::interactive::handlers::commands::tests::plan_todos_commands_follow_extension_lifecycle`
  会偶发失败（约 8 次跑 2 次），在**未改动**的 HEAD 上同样复现（8 次跑 2 次），非本轮引入。

---

## 九、实施记录（MCP 配置面 2.8–2.12，已完成）

| # | 项 | 改动位置 | 关键测试 |
|---|---|---|---|
| 2.8 | 项目层局部覆盖 + 最小写回 | `src/extensions/mcp/config.rs`：`ServerEntry`/`OAuthConfig`/`McpConfig` 的空字段加 `skip_serializing_if`；`set_disabled` 去掉 `inherited` 参数、只写覆盖条目；`/mcp enable\|disable` 见 `src/extensions/mcp.rs`；CLI `enable/disable` 先校验服务器存在 | `project_partial_override_merges_into_global_server`、`set_disabled_writes_minimal_override`、`command_enable_disable_writes_minimal_override` |
| 2.9 | `oauth.clientRegistration: "cimd"` | `config.rs`：`ClientRegistration` + `client_metadata_url`；`oauth.rs`：`with_client_metadata_url`；CLI `--client-registration` / `--client-metadata-url` | `oauth_client_registration_and_metadata_urls_are_validated`、`new_flags_populate_server_fields` |
| 2.10 | `oauth.authServerMetadataUrl` | `config.rs`：字段 + URL 校验；`oauth.rs`：`fetch_authorization_metadata()` + 绕过 `start_authorization`（见 2.10 条目说明） | `oauth_client_registration_and_metadata_urls_are_validated`（配置面；运行时未端到端验证） |
| 2.11 | `auth: {provider}` | `config.rs`：`AuthSetting::Provider` + `auth_provider()` + `auth_mode()` 归入 bearer + 非 loopback https + 项目层禁用；`core/auth.rs`：`resolve_provider_token()`；`manager.rs`：`bearer_token()`（连接时解析）；CLI `--auth-provider` | `provider_auth_requires_https_outside_loopback`、`project_config_rejects_auth_provider`、`resolve_provider_token_prefers_oauth_then_api_key`、`auth_provider_flag_sets_provider_auth` |
| 2.12 | `description` | `config.rs`：字段；`mcp.rs`：`server_summary()` 进 `mcp` 工具说明/snippet、`described()` 进 `/mcp status\|list`；CLI `--description` + `list` 输出 | `server_summary_lists_described_servers`、`description_survives_redaction`、`new_flags_populate_server_fields` |

- **顺带修掉的坑**：`ServerEntry` 的 `None`/空集合字段此前会被序列化成 `null`/`[]`/`{}`，写回任意作用域后
  都会在递归 `deep_merge` 里把继承来的同名字段擦掉（不只是项目层覆盖，`put_server` 也一样）。
  加 `skip_serializing_if` 后写出的配置只含被显式设置的字段，项目层覆盖条目形如 `{"disabled": true}`。
- **端到端验证**：2.9 / 2.10 / 2.11 的运行时路径由 `examples/mcp_test_server/`（真实 HTTP 服务端，
  rmcp server 侧 + hyper）与 `tests/mcp_oauth_e2e.rs`（4 条用例）覆盖：
  - 2.11：`/mcp` 校验 `Authorization: Bearer`，provider token 能连上并调到工具；未登录的 provider 在连接前报错；
  - 2.10：配置 `authServerMetadataUrl` 后授权端点来自配置文档（`/oauth/authorize`），未配置时是合成端点；
  - 2.9：CIMD 的 `client_id` == metadata 文档 URL；动态注册的 `client_id` == 服务器签发值；
  - 服务端自身：缺 token 时 401、带 token 时 initialize 成功（curl 手测同样通过）。
  服务端实现放 `examples/` 而非 `src/`：rmcp 的 server 侧 feature 与 `tower` 只在 dev-dependencies 里，
  不进 lib 的依赖图；`examples/mcp_test_server/main.rs` 是可执行壳（`cargo run --example mcp_test_server`）。
  2.10 仍依赖 rmcp 3.1.4 的内部行为（`start_authorization` 无条件覆盖 metadata），升级 rmcp 时必须重看。
- **文档同步**：`assets/docs/extensions/mcp.md` 补 `description` / `auth.provider` /
  `oauth.clientRegistration` / `clientMetadataUrl` / `authServerMetadataUrl` / 新 CLI flag /
  `/mcp enable|disable` 与「覆盖条目只写最小字段」的说明。
- **验证**：`cargo test` 全绿（lib 2270 项 + 集成测试，含 `mcp_oauth_e2e` 4 条）；`cargo fmt --check` 通过；
  `cargo clippy --all-targets` 只剩改动前就存在的 7 条 warning（`settings_manager` 1、`tasks/widget` 2、
  `examples/bot` 4），新增的 example / 测试零 warning。

---

## 十、实施记录（§一/§三 剩余 bug 项 + 目录全量同步，已完成）

先清掉 §一/§三 中尚未落地的 bug 项，再做第四节的全量目录 diff；均为 bug 修复，未动 §二 的新增功能。

| # | 项 | 改动位置 | 关键测试 |
|---|---|---|---|
| 1.20 | Together `-0813` 丢 thinking 控制 | `assets/models/`（全量重同步 pi-ai 1.0.2） | `scripts/sync-models.py --check` 0 差异 |
| 1.11 | 扩展注册 provider + 已存凭据的默认模型 | 无代码改动（核实无此问题） | `saved_default_model_resolves_for_extension_registered_provider` |
| 1.12 | 远程目录合并平方复杂度 | `model_resolver::provider_models` 动态合并改 `HashMap` 索引 | `dynamic_catalog_merge_upserts_by_key_without_scanning` |
| 1.25 | 扩展命令非法名 | `core/extensions/registry.rs`：`valid_name()` + `registration_issues()`（原 `name_collisions`）；`registered_commands` / `command_provider` / `command_busy_safe` 同步过滤 | `invalid_command_names_are_reported_and_ignored` |
| 1.27 | 恢复会话丢 deferred 工具 | `core/extensions.rs::restore_deferred_activations()`；`Agent::new` + `worker::apply_fork_result` 调用 | `restore_activations_from_restored_history` |
| 1.29 | `/login` `/logout` 状态与类型标签 | `handlers/auth.rs`：`auth_type_label()` / `show_auth_type_labels()`；`core/login_registry.rs::is_subscription()` | `logout_panel_labels_mixed_credential_types` |
| 1.30 | Apple Terminal 启动 logo | `utils/terminal_caps.rs::is_apple_terminal()`；`extensions/banner.rs::render_wordmark()` | `apple_terminal_uses_text_wordmark`、`apple_terminal_needs_macos_and_term_program` |
| 3.4 | provider session UUID 覆盖面 | `provider::session_affinity_headers()`（新增）、`ModelConfig`/`ModelEntry` 增 `sendSessionAffinityHeaders`/`sessionAffinityFormat`；anthropic / completions / responses 接线；网关在客户端自带会话头时强制开启 | `session_affinity_headers_follow_api_and_compat` |

- **行为口径变化**（升级后可能影响上游看得到的请求头）：
  - 非 openrouter 的 openai-compatible provider 不再无条件收到 `x-session-affinity`；
    opencode 的 `openai-responses` 模型（目录 `openai-nosession`）改发 `x-client-request-id`，
    不再带 `session_id`；anthropic 系只有在 `compat.sendSessionAffinityHeaders` 为 true
    （prux 目录：fireworks）时才发 session 头。以上均为对齐 pi 1.0.2 的结果，
    测试与代理网关的用例已同步修正。
- **尚未动（此轮当时的记录；2.3 / 2.7 / 2.13 与 2.2 第一步见第十三节，2.2 第二步与 2.16 已明确不做）**：
  §二的 2.2、2.3、2.7、2.13（设计说明见第十二节）；
  §一的 1.22/1.23/1.24（文档自判「无需改动/结构性偏差」）。
  §二其余项 2.1/2.4/2.6/2.15/2.17/2.18/2.19 见第十一节。
- **验证**：`cargo test` 全绿（lib 2278 项 + 集成测试）；`cargo fmt --check` 通过；
  `cargo clippy --all-targets` 仍只剩改动前就存在的 7 条 warning。

---

## 十一、实施记录（§二 第二批：2.1 / 2.4 / 2.6 / 2.15 / 2.17 / 2.18 / 2.19，已完成）

按「小改动、可测、低回归」挑出的第二批；2.16 经确认**不做**，2.2 / 2.3 / 2.7 / 2.13 只出设计说明（第十二节）。

| # | 项 | 改动位置 | 关键测试 |
|---|---|---|---|
| 2.1 | `samplingParamsByThinkingLevel` | `model_resolver.rs`：`ModelEntry`/`ModelConfig` 新字段 + `apply_model_override` 逐级别逐键合并；`provider.rs`：`ModelConfig::sampling_params_for_level` + `apply_level_sampling_params`；`completions.rs` / `responses.rs` 请求体接线 | `sampling_params_by_thinking_level_parses_and_merges_per_level`、`sampling_params_for_level_uses_clamped_level`、`level_sampling_params_override_named_fields`、`sampling_params_follow_thinking_level_overrides` |
| 2.4 | codemode `models.generateImages()` | `extensions/codemode.rs`：`generate_images` 全局 + `resolve_model_call` / `images_context_arg` / `begin_model_call` / `finish_model_call` / `unshown_images_note`；`description.rs`：`ImagesContext`/`ImagesResult` 与签名 | `script_generate_images_validates_arguments`、`images_context_requires_non_empty_blocks`、`unshown_images_note_only_when_nothing_shown`、`resolve_model_call_uses_only_provider_and_id` |
| 2.6 | codemode 近似名报错 + `store()` 用途说明 | `codemode/glue.js`：`guard()` Proxy 包 `tools` 与全局命名空间、`STORE_HINT`；`codemode.rs`：`models.*` 模型/上下文错误文案 | `missing_tool_member_names_close_matches`、`store_limits_explain_what_the_store_is_for` |
| 2.15 | `quietStartup` | `settings_manager.rs`：`QuietStartup` + 读写；`modes/interactive.rs` 启动接线；`settings_selector.rs` + `handlers/settings.rs` 面板项 | `quiet_startup_reads_booleans_and_header_string`、`settings_list_matches_pi_order_and_skips_unsupported` |
| 2.17 | 系统提示 `mcp_servers` 段 | `extensions/mcp.rs`：`servers_section` / `render_servers_section`；`core/system_prompt.rs`：默认与自定义提示词两条路径都追加 | `servers_section_lists_indirect_servers`、`mcp_servers_section_lists_indirect_servers` |
| 2.18 | 登录面板复制授权 URL | `core/keybindings.rs`：`KeybindingsManager::display_for` / `key_display`；`handlers/panels.rs` 转发 `app.message.copy`；`handlers/mouse.rs::copy_oauth_authorization_url`；`render/panel.rs` 提示行 | `login_panel_app_message_copy_copies_the_url`、`display_for_renders_the_first_binding` |
| 2.19 | URL 折行后可点击 | **无代码改动**（见下） | 现有 `login_oauth_variants_have_exactly_one_blank_line_around_content` |

- **2.1 口径**：级别先用模型能力钳制（对齐 pi `clampThinkingLevel`），再按「目录 `samplingParams` → 该级别覆盖项」取值；
  **目录值只补请求体未设的键**（prux 既有偏差），**级别覆盖项按 pi 语义后写覆盖**（含 `temperature`）；
  `settings.json` 的 `temperature` 是用户级覆盖，级别覆盖项不改写它。prux 没有独立的 azure api，
  azure 端点走 `openai-completions` / `openai-responses`，因此已被同一路径覆盖。
- **2.4 行为**：`models.generateImages(model, context)` 与 `classify` 同一套簿记（记录进 `details.modelCalls`、
  用量并入 codemode 结果、4 并发闸门）；只认 `provider`/`id`，参数形状不对抛错，provider 侧失败不抛。
  脚本生成了图片但一张都没用 `image()` 展示时，宿主在成功结果里补一条提示（对齐 pi）。
- **2.6 行为变更（注意）**：读 `tools.<不存在>` 或命名空间不存在的成员由「返回 `undefined`」改为
  **抛 `TypeError` 并列出近似名**；探测成员需要 `"name" in tools`。这是 pi 1.0.0 的刻意变更，脚本若靠
  `tools.x === undefined` 判存在会受影响。
- **2.15 默认值偏差**：pi 的默认是 `false`（每次启动都输出资源清单）；prux **未设置时按 `"header"`**
  （保持「默认不输出清单、只显示横幅」的既有行为），要清单就显式写 `false` 或加 `--verbose`。
  `--verbose` 在任何档位下都强制两者都显示。
- **2.17 范围**：只列工具**不直接声明**的服务器（`codemode` / `codemode-deferred` / `deferred`，含
  `toolExposure` 命中）；默认 `direct` exposure 的服务器仍只出现在 `mcp` 工具的说明里。段整体 4096 字符、
  单台摘要 250 字符上限（对齐 pi），放不下时末尾省略并计数。「不阻塞首个 prompt」prux 本就不阻塞（连接惰性）。
- **2.19 结论与顺带发现**：
  - OSC8 超链接**无法穿过 ratatui**：`Buffer::set_stringn` 会丢弃含控制字符的 grapheme（把 ESC 滤掉、把
    `]8;id=none;…` 其余部分当普通文本写入），因此 pi 那种「span 里塞 OSC8」的做法在 prux 的渲染路径上不可用。
  - prux 的做法是**鼠标命中区域**：`handlers/mouse.rs::handle_panel_oauth_ctrl_click` 按
    `oauth_url_lines(url, width)` 算出折行后的 URL 行范围，Ctrl+click 命中任意一行都打开浏览器，
    功能上已满足 2.19 的目标（长 URL 折行后可点）。
  - **既有 bug（本次未改，建议单独处理）**：`render/markdown.rs` 的 `osc8()` 分支在
    `hyperlinks_enabled()` 为真时会打印 `]8;id=none;<url><label>]8;;` 字面量（实测：60 列终端里
    `[example](https://example.com/x)` 渲染成 `]8;id=none;https://example.com/xexample]8;;`）。
    两条修法：① 删掉该分支（链接退化为纯文本，最省事）；② 在 backend 层做后处理（自定义 `Backend`
    包装 `CrosstermBackend`，绘制前把哨兵字符换成 OSC8 序列），可同时救活面板与 markdown 的超链接。
    ② 才是真正的「折行后可点击」，但要动终端输出层。
    **（第十四节已推翻此结论**：不需要自定义 `Backend`，`Cell::set_symbol` 本身就能承载 OSC8，见 2.19；
    方案 ② 的「哨兵字符 → 绘制时换成 OSC8 序列」思路被保留，只是落点从 backend 移到了 buffer 定型后。）
- **验证**：`cargo test` 全绿（lib 2292 项 + 19 个集成测试目标）；`cargo fmt --check` 通过；
  `cargo clippy --all-targets` 只剩改动前就存在的 7 条 warning（`settings_manager` 1、`tasks/widget` 2、`examples/bot` 4）。

---

## 十二、设计说明（2.2 / 2.3 / 2.7 / 2.13）

这四项当时的落点、接口与代价；**落地的是 2.3 / 2.7 / 2.13 与 2.2 第一步**，实现记录见第十三节；
**2.2 第二步、2.16 经确认不做**（下面 12.1 保留当时的成本分析，仅作决策留痕）。

### 12.1（对应 2.2）Anthropic workload identity federation

- **pi 的做法**：`ANTHROPIC_FEDERATION_RULE_ID` + `ANTHROPIC_ORGANIZATION_ID` + `ANTHROPIC_IDENTITY_TOKEN_FILE`
  三者齐全（`ANTHROPIC_SERVICE_ACCOUNT_ID` / `ANTHROPIC_WORKSPACE_ID` 可选）且**没有任何请求级凭据**时，
  把这组值当「provider env」交给 `@anthropic-ai/sdk`：SDK 读 identity token 文件 → 调 Anthropic 的
  OIDC federation 端点换短期 access token → 自动刷新并缓存（pi 为省 token 缓存而**跨请求复用 client**）。
  优先级最低：API key、`ANTHROPIC_AUTH_TOKEN`、`ANTHROPIC_OAUTH_TOKEN` 都赢它。
- **prux 现状**：`core/auth.rs` 只认 `ANTHROPIC_API_KEY` / `ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_OAUTH_TOKEN`；
  `provider/anthropic.rs` 自己拼 reqwest 请求（`x-api-key` 或 bearer），没有 SDK。
- **落点（分两步；第 2 步已确认不做，以下仅作决策留痕）**：
  1. **只做识别与报错（低成本，已做）**：`auth.rs` 增加「federation 已配置但当前版本不支持」的判定，
     `/login`、启动诊断与 `ProviderStatus` 错误文案里明确说明需要什么；避免用户配了三元组却看到
     「No API key for provider: anthropic」这类误导信息。
  2. **自己实现交换（成本高，明确不做）**：需要先确认 SDK 的交换端点与请求体（SDK 源码不在本仓库，得从
     `@anthropic-ai/sdk` 的 tarball 或抓包实测确认：端点路径、`identity_token` 的传递方式、
     `service_account_id`/`workspace_id` 的字段名、返回体的 token 字段与 `expires_in`）。
     落点：`core/auth.rs` 里新增 `FederationConfig`，`provider/anthropic.rs` 取 token 的地方改成
     「无 key/token → 取 federation token」，token 缓存放 `OnceLock<Mutex<HashMap<Key, CachedToken>>>`，
     按 `expires_in - 60s` 预刷新；identity token 文件每次交换前重读（K8s 会轮转）。
- **测试**：① 造 env + 临时 identity 文件，断言凭据来源判定与文案；② 交换流程用本地 HTTP 桩
  （沿用 `tests/web_access.rs` / `examples/mcp_test_server` 的桩服务端套路）断言请求体、缓存命中与刷新。
- **风险/工作量**：① S；② M（难点全在「端点与字段必须实测确认」，猜错就是白做）。
  第 2 步**已决策不做**：无 SDK 可依、端点靠实测、只覆盖 K8s 一类场景，性价比不成立。

### 12.2（对应 2.3）Anthropic OAuth「copy code login」

- **pi 的做法**：登录时先选 **Browser login**（默认，本地回调）或 **Copy code login**；后者
  `redirect_uri = https://platform.claude.com/oauth/code/callback`，Anthropic 页面展示 `code#state`，
  用户整段粘回 pi，pi 拆出 `code` 与 `state` 后走 token 交换。
- **prux 现状**：`core/oauth/anthropic.rs` 固定绑定本地回调端口，1.5 之后端口被占用会**直接报错**
  （不再静默退化成「粘贴 redirect URL」）——也就是说现在**没有**手动登录路径，这个缺口正好由 copy-code 补上。
- **落点**：
  - `core/oauth.rs::LoginKind` 增一个变体（如 `AuthorizeCopyCode { redirect_uri, pkce, state }`），
    `anthropic.rs` 提供 `start_authorize_copy_code_flow()`（不绑端口、用 `platform.claude.com` 回调地址）；
  - 登录面板第一步加选择项（浏览器 / 复制授权码），选择结果进 `App.oauth_login` 的步骤状态机；
  - 粘贴行解析：接受 `code#state`、整条 redirect URL（保留现有兼容）与裸 `code`；
  - 凭据写入沿用 `oauth.rs` 的 `OAuthCredential`（`expires_at`、refresh token 等字段已具备）。
- **测试**：`code#state` / redirect URL / 裸 code 三种粘贴输入的解析、redirect_uri 出现在授权 URL 里、
  token 交换请求体断言（桩服务端）。
- **风险/工作量**：S–M。风险点：Anthropic 侧该 redirect_uri 是否需要额外注册（pi 已内置，按 pi 的常量照抄即可）。

### 12.3（对应 2.7）codemode 描述精简

- **pi 1.0.0 的压缩手法**（`extensions/codemode/tool.ts`）：描述 = 3 行 INTRO + 3–4 行 globals 摘要 +
  工具声明段；`models.*` 的**类型与签名整段不再写进描述**，只留一行
  ``- `models`: classifiers and image generation. Read <docs/codemode.md> first.``，模型需要时用 read 工具取文档。
- **prux 现状与量级**（`extensions/codemode/description.rs`）：
  | 段 | 字符 | ≈token |
  |---|---|---|
  | `INTRO`（脚本全局/助手/输出语义，25 行 bullet） | 3147 | ~790 |
  | `MODEL_API`（`ModelInfo` / `ClassifierContext` / `ClassifierAnswer` / `ClassifierResult` / `ImagesContext` / `ImagesResult` 类型 + 5 个签名） | 3378 | ~845 |
  | `DEFERRED_TOOLS_GUIDANCE` | 299 | ~75 |
  合计约 6500 字符 ≈ 1600 token 的固定开销，另加工具声明段（默认 `inlineBudget` 3000 token）。
- **建议改法**：
  1. `INTRO` 压到 6 行以内：合并「沙箱是什么」「`tools.x` 返回/失败语义」「`// @options` 首行」，
     其余细节（内存上限、无定时器、取消语义）移到 `docs/extensions/codemode.md`。
  2. `MODEL_API` 换成「一行签名清单 + 文档指引」：保留
     `models.getModelsOfType/getAvailableOfType/getModelOfType/classify/generateImages` 的名字与一行签名
     （否则 `doc_helper` 开启、`read` 被关掉时模型无路可走），类型定义移入文档。
     文档路径用 `core::system_prompt` 已有的安装目录 docs 路径（`<install_dir>/docs/extensions/codemode.md`），
     与 pi 的 `CODEMODE_DOCS_PATH` 同思路。
  3. 同步补文档（`assets/docs/extensions/codemode.md` 已在本轮加了 `models.*` 与限制段），
     并把 `description.rs` 的断言测试改成断言「包含文档路径与签名行」而不是完整类型块。
- **收益/风险**：固定开销从 ~1600 token 降到 ~400 token 量级（接近 pi「5300→3300」的比例）；
  风险是模型在需要类型细节时多一次 `read`（在 `doc_helper`/`read` 不可用时用签名行兜底）。
- **工作量**：S（纯文案 + 断言调整），但**需要你先认可「把类型定义搬出描述」**这个取舍。

### 12.4（对应 2.13）`registerToolRenderer()`

- **pi 的做法**：扩展可用 `pi.registerToolRenderer()` 给「未注册渲染器的工具」（典型是 MCP 工具）提供
  渲染函数，宿主在渲染工具结果时优先用扩展渲染器。
- **prux 现状**：扩展 API（`core/extensions.rs`）有 `exposure`/`namespace`/`annotations`/`output_schema`/
  `prepare_loadout`/`execute_tool`，**没有**渲染回调；工具结果渲染是宿主内置的
  （`modes/interactive/render/messages.rs` 按工具名分派，MCP 走单一 `mcp` 工具的折叠逻辑）。
- **接口提案**（同步、无 agent 锁，符合 TUI 线程约束）：
  ```rust
  /// 扩展为某个工具提供的渲染器（宿主按工具名分派；返回 None 时交回内置渲染）。
  pub trait ToolRenderer: Send + Sync {
      /// 渲染器名（面板 / 诊断用）
      fn name(&self) -> &'static str;
      /// 生效的工具名（如 "mcp"、"subagent"）
      fn tools(&self) -> &[&'static str];
      /// 同步渲染：拿到工具名、参数、结果（文本 + 结构化）、状态与宽度，产出确定行数与折叠策略
      fn render(&self, ctx: &ToolRenderCtx<'_>) -> Option<ToolRender>;
  }
  pub struct ToolRender { pub lines: Vec<Line<'static>>, pub preview_lines: Option<usize> }
  ```
  - 注册：`core::extensions::register_tool_renderer(owner, renderer)`，随扩展启停生效（禁用即回退内置）；
  - 调用点：`render/messages.rs::render_tool_output` 第一步查扩展渲染器；行数必须由渲染器**确定给出**
    （与 `MessageCache`/高度计算同一口径，避免 1.6 那类「折叠按逻辑行还是视觉行」的歧义）；
  - 缓存：沿用 `MessageCache`，键加渲染器版本（扩展 reload 后失效）；
  - 输入里**不给** `ToolExecCtx`/agent 句柄，避免扩展在渲染路径里做事。
- **测试**：注册一个假渲染器 → 断言行/折叠 / 禁用后回退 / 宽度与主题变化后缓存失效。
- **风险/工作量**：M–L。它是**扩展 API 的形状决策**，一旦发布就要兼容，建议先只给内置扩展用（不对外承诺稳定）。
- **已按本提案落地（第十三节）**，与大方向的两处差异：① 渲染行的样式用 prux 自有的
  `RichLine = Vec<RichSpan>`（`core` 不依赖 ratatui，与扩展 UI 通知同口径）；② 调用点放在
  `render_tool_block` 的第一步（整块接管，宿主的 `edit` diff 不参与），而不是只接管输出段。

---

## 十三、实施记录（§二 最后四项 + OSC8 字面量 bug，已完成）

第十二节四项的落地部分一次做完（2.2 只落第一步，**其第二步已确认不做**），另修掉第十一节记下的 OSC8 字面量 bug。

| # | 项 | 改动位置 | 关键测试 |
|---|---|---|---|
| 2.2（第一步） | federation 识别与报错 | `core/auth.rs`：`ANTHROPIC_FEDERATION_REQUIRED` / `ANTHROPIC_FEDERATION_OPTIONAL` / `FederationConfig` / `anthropic_federation()` / `federation_note()`；`core/agent_loop.rs` 凭据缺失错误、`modes/interactive.rs` 启动诊断、`handlers/auth.rs` 的 `/login` provider 列表接线 | `anthropic_federation_is_recognized_and_deferred_to_credentials` |
| 2.3 | Anthropic copy-code 登录 | `core/oauth.rs`：`LoginKind::AuthorizeCopyCode` + `supports_copy_code_login` / `start_copy_code_login` + `exchange_login` 增 `redirect_uri` 覆盖；`core/oauth/anthropic.rs`：`COPY_CODE_REDIRECT_URI` / `start_copy_code_flow` / `exchange_body`；`oauth_flow.rs`：`start_copy_code` / `redirect_uri_override`；`panel.rs` + `handlers/panels.rs` + `handlers/auth.rs`：`PanelKind::LoginMethod` 选择步 | `copy_code_flow_uses_provider_redirect_without_callback_server`、`exchange_body_follows_redirect_override`、`anthropic_copy_code_login_skips_callback_server`、`login_oauth_copy_code_panel_asks_for_code_state` |
| 2.7 | codemode 描述精简 | `extensions/codemode/description.rs`：`INTRO` 压到 6 行、`MODEL_API_SIGNATURES` / `MODEL_API_CONVENTIONS` / `model_api_section()` / `intro_section()` / `codemode_docs_path()`；类型定义搬进 `assets/docs/extensions/codemode.md` | `fixed_overhead_stays_small`、`description_includes_model_api_signatures_and_docs_path` |
| 2.13 | 扩展工具渲染器 | `core/extensions/renderers.rs`（新增）：`ToolRenderer` / `ToolRenderCtx` / `ToolRender` / `RichLine` / `RegisteredToolRenderer` + 注册表；`core/extensions.rs`：`Extension::tool_renderers()`；`core/extensions/registry.rs` 注册/注销接线；`render/messages.rs`：`render_extension_tool_block` + `MessageCache` 渲染器指纹；`assets/docs/extensions.md` 新增「工具渲染器」段 | `renderer_follows_extension_activation`、`last_registration_wins_and_context_is_forwarded`、`extension_renderer_takes_over_tool_block_and_falls_back` |
| 既有 bug | markdown OSC8 字面量 | `render/markdown.rs`：删掉 OSC8 分支（ratatui 的 buffer 会吃掉 ESC，只剩 `]8;id=none;…` 文本）；`utils/terminal_caps.rs`：`hyperlinks_enabled()` 注明当前无调用方、留给将来的输出层修复 | `markdown_links_render_as_visible_text_without_escape_sequences` |

- **2.2 边界（重要）**：只做「识别 + 报错」。`ANTHROPIC_FEDERATION_*` 三元组齐备且**无请求级凭据**时，
  错误与启动提示会说明「已配置但本版本不支持」并点出 identity token 文件是否可读；
  有 `ANTHROPIC_API_KEY` / `AUTH_TOKEN` / `/login` 凭据时凭据优先、不提 federation。
  真正的 OIDC 交换（读 token 文件 → 换短期 access token → 缓存刷新）**明确不做**：
  端点与字段必须先实测确认（12.1 第二步），成本与收益不匹配，不再排期。
- **2.3 行为**：`/login` → account → Anthropic 会先出 `LoginMethod` 面板（Browser login 默认 / Copy code login）。
  copy-code 流不绑端口（可同时开多个），授权 URL 的 `redirect_uri` 是
  `https://platform.claude.com/oauth/code/callback`，token 交换回传同一个值；
  粘贴 `code#state` 后按 state（=PKCE verifier）校验。浏览器流与端口占用时的粘贴回退**保持原样**。
- **2.7 口径**：固定开销 ~1600 → ~460 token（`MODEL_API` 的类型定义与 `INTRO` 的细节进
  `docs/extensions/codemode.md`）；描述里保留五个一行签名与文档绝对路径，
  `read` 不可用（`codemode.mode: only`）时模型仍有签名行可依。
- **2.13 口径**：渲染器是**同步、无 I/O** 的扩展回调；宿主负责铺背景、按宽度折行与裁剪，
  渲染器只需给行与 `preview_lines`（视觉行）；`edit` 的内置 diff 渲染不参与接管；
  同一工具名以最后一次注册为准；禁用扩展或 `/reload` 后 `MessageCache` 按渲染器指纹整表失效。
- **未改**：`hyperlinks_enabled()` 与终端能力探测保留（真修 OSC8 要在输出层做，见第十一节 ②）。
- **验证**：`cargo test` 全绿（lib 2302 项 + 全部集成测试）；`cargo fmt --check` 通过；
  `cargo clippy --all-targets` 仍是改动前那 7 条 warning（`settings_manager` 1、`tasks/widget` 2、`examples/bot` 4）。

---

## 十四、实施记录（2.11 实时读 token + 2.19 OSC8 超链接，已完成）

这两项在第十一/十二节曾判为「不做」，本轮**推翻结论并落地**——两项都是找到了更短的实现路径，
不是绕开问题。

| # | 项 | 改动位置 | 关键测试 |
|---|---|---|---|
| 2.11 | `auth.provider` token 每次请求现取 | `extensions/mcp/manager.rs`：`LiveTokenClient<C>` + `StreamableHttpClient` 实现（5 个方法）、`live_token_error`、`connect` 接线（静态配置不再预填 token）；`Cargo.toml` 加 `sse-stream`（trait 签名里的 `BoxStream<Result<Sse, SseError>>` 需要它） | `provider_auth_rereads_token_on_every_request`（e2e：连上后换凭据 + 服务端同步换校验值 → 下一发请求成功；删凭据 → 立刻报错）、`provider_auth_connects_with_provider_token` |
| 2.19 | 登录 URL 发 OSC8 | `render/hyperlink.rs`（新增）：`LINK_START`/`LINK_END` 哨兵、`LinkPass` widget、`apply`、`strip`；`render.rs`：`render_frame` 拆出 `render_frame_widgets` 并在末尾调 `apply`；`panel.rs`：`link_line` 给授权 URL 与 device-code URI 的每个折行段加哨兵 | `sequences_attach_to_link_text_without_shifting_columns`、`disabled_or_missing_url_only_strips_sentinels`、`line_without_sentinels_is_left_alone`、`login_oauth_url_is_rendered_as_osc8_hyperlink` |

- **2.11 口径**（推翻第十一节的「rmcp 静态 header 做不了」）：
  - rmcp 的 `StreamableHttpClient` 是**可实现**的 trait（`AuthClient` 就是这种包装），所以不必自建
    传输层：`LiveTokenClient` 在每个请求方法里 `resolve_provider_token` 后转发给内层 reqwest。
  - **故意覆盖** `post_message_with_max_sse_event_size` / `get_stream_with_max_sse_event_size`：
    trait 的默认实现会丢掉 `max_sse_event_size`，不覆盖等于把 SSE 事件大小上限关掉。
  - 未登录 / 凭据消失 → `StreamableHttpError::Io(PermissionDenied, "…Run `/login x` first")`，
    错误链上保留 provider 名；不退化成匿名请求。
  - bearerToken / bearerTokenEnv 仍是建连时读一次的静态值（配置语义如此）；OAuth 流转与它无关。
  - `probe_oauth_challenge`（`/mcp login` 的 401 探测）仍用一次性读取的 `bearer_token()`。
- **2.19 口径**（推翻第十一节的「要动终端输出层」）：
  - ratatui 0.30 **给超链接留了路**：`Cell::set_symbol` 可以承载 OSC8 转义序列，配
    `CellDiffOption::ForcedWidth(1)` 声明宽度即可（`ratatui-core` 的 `merge_diff_link` /
    `merge_diff_split_link` 测试就是为这条路径写的）；会丢弃控制字符的只有 text 渲染通道
    （`Buffer::set_stringn`）。
  - 所以不必自定义 `Backend`：渲染层在链接文本首尾放两个哨兵（各占 1 列），`render_frame`
    末尾统一把哨兵列还原成空格、并把 OSC8 起始/结束序列粘到相邻的首/末字符 cell 上
    （可见宽度仍是 1，列对齐不变）。折行后每段各有一组哨兵 → 每段独立可点。
  - 终端不支持 OSC8（`hyperlinks_enabled()` 为假）时只剥哨兵，退化为纯文本；
    代价是链接行左右各多 1 列空白（原来放哨兵的位置）。
  - 消息区 markdown 链接**仍未**发 OSC8（那条路径要穿 `MessageCache` 与滚动裁剪，收益小），
    可点击性继续由鼠标命中区域承担。
- **文档同步**：`assets/docs/extensions/mcp.md`（`auth.provider` 改为「每次请求现取」）、
  `utils/terminal_caps.rs::hyperlinks_enabled` 注释（不再是「无调用方」）、
  `render/markdown.rs::render_link_spans` 注释（指向新的 `hyperlink` 模块）。
- **验证**：`cargo test` 全绿（lib 2306 项 + 全部集成测试，含 `mcp_oauth_e2e` 5 条）；
  `cargo fmt --check` 通过；`cargo clippy --lib --tests` 只剩改动前就存在的 3 条 warning
  （`settings_manager` 1、`tasks/widget` 2）。

---

## 十五、实施记录（消息区 markdown 链接可点击，已完成）

第十四节只让登录面板的授权 URL 发了 OSC8；消息区的 markdown 链接既没发 OSC8、也没有鼠标命中区，
所以点下去没有任何反应。本轮按方案 C 把两路都接上：同一份「链接区域」元数据，
既用来在 buffer 里注入 OSC8（终端原生点击），也用来做 `ctrl+click` 命中（不依赖终端）。

| 项 | 改动位置 | 关键测试 |
|---|---|---|
| 链接区域收集 | `render/markdown.rs`：`LinkSpan` + `collect_link_spans` + `link_target` | `link_spans_cover_visible_text_and_target`、`wrapped_link_yields_one_span_per_line_with_same_url`、`bare_link_uses_its_text_as_target`、`bare_email_link_target_gets_mailto_prefix`、`adjacent_links_stay_separate`、`no_links_means_no_spans` |
| 行号贯通缓存 | `render/messages.rs`：`CachedMsg.links`、`HistoryRender.links`、`render_timeline_item` / `render_streaming` 出参；`render/history.rs` 累加全局行号 | `links_from_several_messages_keep_their_own_rows` |
| 屏幕换算与注入 | `render.rs::render_message_area`：内容行 → 屏幕行列存 `app.mouse.links`；`render/hyperlink.rs`：`apply` 增加显式区域注入（`attach` / `osc8_start`） | `message_links_become_clickable_rows`、`explicit_regions_attach_sequences_to_first_and_last_cell`、`disabled_explicit_regions_are_left_untouched` |
| Ctrl+click 命中 | `app.rs`：`LinkRow` + `MouseSel::links` / `link_at`；`handlers/mouse.rs`：`open_message_link`（Ctrl+左键优先于选择/折叠） | `message_links_become_clickable_rows`（命中与不命中断言） |

- **定位口径**：链接区域在**最终行**上事后扫描，而不是渲染期往 span 里塞元数据：
  - 识别链接靠「前景色 == `mdLink` + `Modifier::UNDERLINED`」。不能整份 `Style` 相等，
    因为消息块会给整行 `patch` 背景色（`bg_line`，user 消息 / custom message 都走它）；
  - ` (url)` 提示的样式与**代码块边框完全相同**（默认主题下都是 `fg(Reset).dim()`），
    所以只能按「紧跟在链接文本之后、以 `(` 开头、到右括号结束」的内容形态识别；
  - 折行会把链接文本与 ` (url)` 切成多行多段：按「同一行靠后、或紧接下一行」判为同一链接，
    同一行的多段合并成一个区间；行号相对条目起点，再经 `history` 累加、`render` 视口换算成屏幕坐标；
  - 行偏移全部在**出口扫描**，所以块内缩进（`pad_line`）、背景框（`bg_line`）等后处理都已定型，
    列区间不需要各处手工补偿。
- **两条点击路径为什么都要**：开着 mouse capture 的 TUI 里，`cmd/ctrl+click` 到底被终端自己吃掉
  还是上报给应用，各终端不一致。支持 OSC8 的终端由终端打开（prux 不参与）；不支持 OSC8、
  或事件被上报时由 prux 的 `ctrl+click` 命中区打开。两路共用同一份 `app.mouse.links`，行为一致。
- **范围**：只覆盖 markdown 链接（`[文本](url)`、`<url>`、邮箱）；工具输出里的裸 URL、
  代码块内的 URL 不算（与 pi 一致）。
- **验证**：`cargo test` 全绿（lib 2316 项 + 全部集成测试）；`cargo fmt --check` 通过；
  `cargo clippy --lib --tests` 只剩改动前那 3 条 warning（`settings_manager` 1、`tasks/widget` 2）。

---

## 十六、实施记录（2.25 终端内联图片，已完成）

2.25 原先判「结构性不适用」（prux 只画 `[image]` 占位）。重评后的结论是**可行但成本高**：
它不是开关而是一套子系统，所以按用户要求选「引入 `ratatui-image` + settings.json 开关
（默认关闭）」这条路径落地——关闭时保持既有渲染，开启时才付出探测与编码成本。

| 项 | 改动位置 | 关键测试 |
|---|---|---|
| 依赖 | `Cargo.toml`：`ratatui-image = "11.1"`（`default-features = false, features = ["crossterm"]`，不带 chafa） | — |
| 能力探测与几何 | `src/utils/terminal_image.rs`（新增）：`prime_image_picker` / `image_picker` / `image_font_size` / `fit_cell_size` / `image_dimensions` / `decode_image` / `image_key` + `PRUX_IMAGE_PROTOCOL` 覆盖 | `fit_cell_size_keeps_aspect_within_the_box`、`fit_cell_size_uses_font_metrics`、`fit_cell_size_rejects_empty_boxes`、`fit_cell_size_never_returns_zero_for_a_visible_box`、`image_key_follows_content` |
| 设置 | `settings_manager`：`read/write_settings_show_images`（缺失默认 **false**）；`/settings` 新增 Show images 项与 `apply_change_show_images` | `show_images_defaults_to_false_and_round_trips`、`settings_list_matches_pi_order_and_skips_unsupported` |
| 槽位收集 | `render/messages.rs`：`ImageLayout` / `ImageSlot` / `image_cell_size` / `push_image_block`；`render_message_blocks` / `render_assistant` / `render_tool_block_inner` / `render_timeline_item` 增参；`CachedMsg.images` / `HistoryRender.images`；用户消息图片（此前被静默丢弃）与工具结果图片（`ToolResultView.images`）都接上 | `image_block_reserves_rows_and_reports_slot`、`image_block_falls_back_to_placeholder_when_disabled`、`image_block_falls_back_when_data_is_not_an_image`、`user_message_images_are_reserved_after_the_text`、`tool_result_images_are_reserved_inside_the_tool_block` |
| 绘制 | `render/image.rs`（新增）：`ImageCache`（按内容指纹缓存已编码协议，16 条 / 32 MiB 上限，LRU 淘汰）+ `ImageRect`；`render.rs`：`render_message_area` 把槽位换算成屏幕区域（含视口裁剪），`render_frame` 末尾在超链接之后用 `SlicedImage` 绘制 | `show_images_renders_inline_image_cells`、`inline_image_is_clipped_when_scrolled_half_out` |
| 缓存失效 | `App::set_show_images`（清消息缓存 + 协议缓存）、`ensure_cache_params` 的图片指纹、`HighlightSnapshot.images` 与 `HighlightDone.images` 提交前校验 | 上面两条渲染测试（关闭 ↔ 开启切换） |

- **口径**：
  - **关闭（默认）**：图片块**整块不渲染**（既不预留行，也不留 `[image]` 占位行——占位行
    本身也是一行视觉噪音），不探测终端、不占用启动时间、不动终端；开关切换会整表失效消息缓存
    与协议缓存。只有「开了开关但图片数据坏了 / 宽度不够」才回落到 `[image]` 占位行。
  - **宽度上限 60 列**、高度上限取「同宽正方形」换算的行数（对齐 pi 的 `imageWidthCells` 默认
    与 `maxHeightCells` 算法）；图片按比例缩放，预留行在渲染前就定好，滚动/裁剪按行偏移。
  - **探测时机**：只在启动时、且只在 `showImages` 为真时做（查询读写 stdin，必须早于
    crossterm 事件流）；运行中通过 `/settings` 打开时改走环境变量判定（协议 + 默认字体尺寸），
    文档已注明「重启一次更准」。
  - **tmux 内用裸 sixel**：tmux 3.4+ 带 sixel 支持（`#{sixel_support}` 为 `1`）时用 sixel，
    否则回落 halfblocks（纯文本，复用器不拦，但只有「列数 × 行数×2」个采样点，必然糊）。
    「裸」是关键：库的 tmux 路径把 sixel 包进 DCS passthrough（`\x1bPtmux;…`），实测
    （tmux 3.6a + foot）会被 tmux 当普通文本打到屏幕上（界面刷花）并阻塞写入（界面假死）；
    库只看构造时的 `TERM` / `TERM_PROGRAM` 判断 tmux，所以构造 picker 时临时把这两个变量
    换成非 tmux 值（`picker_outside_tmux`，进程内一次、立刻还原），顺带也让库不再执行
    `tmux set -p allow-passthrough on`。
    tmux 转发 sixel 仍受 `allow-passthrough` 管辖（没开只会吞掉序列、图片区空白），
    探测到没开时 prux 显式打开当前 pane 的该选项（`tmux set -p`，不动全局配置）。
    单元格像素尺寸必须取真实值：tmux 会把客户端像素尺寸传播到 pane 的 pty，用
    `crossterm::terminal::window_size()` 的 px ÷ 单元格数（本机 136×36 格 / 2992×1692 px
    → 22×47）当 picker 字体，等价于 tmux 的 `#{client_cell_width}`。用库默认的 10×20 会让
    sixel 只占预留区左上角一小块、右下留一大片空（因为 sixel 是按像素画的）。
    `PRUX_IMAGE_PROTOCOL=kitty|iterm2|sixel|halfblocks|none` 可强制，`none` 等于关闭。
  - **预留尺寸必须与协议尺寸逐格一致**：`fit_cell_size` 原来会把小图放大填满盒子，
    而协议库的 `Resize::Fit` 是 `fit_area_proportionally(w, h, min(盒宽, w), min(盒高, h))`
    ——**永不放大**。口径不一致的后果是图片画在预留区左上角、右下留出一大块消息背景色
    （333×299 的粘贴图在 foot 里预留 60×25、实际只画 28×12）。现在逐格镜像库的算法，
    并加了对照库 `SlicedProtocol::size()` 的回归测试。
  - **图片块后留一行 padding**：正文块自带间隔、图片块以前没有，导致多张图片挤在一起、
    最后一张图底部没有 padding；现在 `push_image_block` 预留 `rows` 行，
    且该行与预留行同样式——用户消息带消息背景时要沿用背景色，否则色块中间会断开露出终端底色。
    补不补这一行看图片盒子底部的**透明**补边（库的 `Resize::Fit` 把不足一格的余量补成
    `Rgba([0,0,0,0])`，不显色但占位置）：补边 ≥ 半行时不再补，否则多图之间会空出两行。
  - **缩放用 Triangle**：`Resize::Fit(None)` 默认的 `Nearest` 在源图远大于目标框时是点抽样
    （糊且带锯齿），改传 `FilterType::Triangle`；更清晰的 Lanczos3 一张 1726×778 截图要
    ~500ms，会把首帧卡住，不值当。
- **未覆盖**：会话查看器（`/session` 全屏覆盖层，走 `MessageCache`）与历史导出仍是 `[image]`
  占位；`render_all_messages`（测试辅助）同理。扩展渲染器接管的工具块不参与图片绘制。
- **滚轮作用域**（顺带修的独立缺陷）：原先滚轮只看「不在覆盖层、不在停靠面板内」就滚消息区，
  于是指针在输入框、状态栏、底栏上滚动也会把输出区滚走（在图片把消息区撑高时尤其明显，用户
  报成「输入框在切换历史记录」）。现在 `App::over_log_area` 用最近一帧记录的 `mouse.log_area` /
  `log_area_h` 判定指针是否真在消息区内，不在就不滚。
- **依赖代价**：新增 16 个 crate（`ratatui-image`、`icy_sixel`、`quantette`、`bitvec`、
  `wide`、`rustix`、`self_cell` 等）。sixel 编码器是 ratatui-image 的**非可选**依赖，
  关不掉；不带 chafa（`default-features = false`），halfblocks 用库内置的原始实现。
- **验证**：`cargo test` 全绿（lib 2329 项 + 全部集成测试）；`cargo fmt --check` 通过；
  `cargo clippy --lib --tests` 仍是改动前那 3 条 warning（`settings_manager` 1、`tasks/widget` 2）。
  另注：整包并行跑时 mcp 相关的渲染测试（`non_bash_fold_limits_visual_lines_for_long_single_line`、
  `mcp_call_header_*` 等）会偶发失败：`core/extensions/renderers.rs` 的测试会在**进程级全局**
  注册表里临时注册一个 `mcp` 渲染器，并发跑的其它测试渲染 `mcp` 工具块时会被它接管；
  单独跑必过。已用 `git worktree` 在改动前的 HEAD 上量过基线：4 次全量 lib 测试失败 1 次
  （同一个用例），**非本轮引入**；与本文档第八节记录的另两条偶发失败同源。
  彻底修需要把全局注册表相关测试串行化（本轮未做）。

---

## 附：本文未展开的区间变更（已排除）

- `packages/client`、`packages/protocol`、`packages/telemetry`：本区间无条目。
- `packages/server`：仅 3.2 一条破坏性变更。
- `packages/ai` 的 `packages/ai/src/utils/transcript.ts` 中 `hasToolRedefinitions()` 废弃
  （1.0.1）：prux 无该 API，见 1.28。
- `utils/oauth-page.ts` 彩色 logo（1.0.0：OAuth 浏览器页改彩色 Pi logo）：prux 的
  OAuth 页面（`src/core/oauth.rs:335` 起）本来就没有 logo，只有文字卡片，**不适用**。
- 内置扩展 Built-in 段 / `-builtin:<name>`（属 0.99.0 条目，不在本区间）、
  `defaultTools` 的 `+name`/`-name`（prux 已在 `src/core/settings_manager.rs:499` 实现）。
