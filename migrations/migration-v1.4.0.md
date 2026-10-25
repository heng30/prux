# Prux 迁移指南：pi 1.3.0 → 1.4.0

> **来源**：pi 仓库 tag `v1.3.0` → `v1.4.0` 之间各包 `packages/*/CHANGELOG.md` 的 `[1.4.0]` 段
> （`git log --oneline v1.3.0..v1.4.0` 共 81 个非 merge 提交），并用 `git diff v1.3.0..v1.4.0`
> 对关键条目（重试表、shell 探测、剪贴板、分类器、客户端身份）逐条核对源码。
> prux 基线：pi **1.3.0**（上一份指南 `migrations/migration-v1.3.0.md`，已全部收口）。
>
> **收录规则**（按用户要求）：只收录
> ①「prux 已实现功能的 bug 修复」与
> ②「1.3.0→1.4.0 区间内 pi 新增/变更/移除的功能」。
> prux 没有、且在本区间之前就已存在的 pi 功能（Bedrock / llama.cpp / `openai-codex` /
> `packages/{chord,durable,env,telemetry}` 的既有能力 / Node 与 Nix 打包面）**不逐条列出**，只在第五节汇总。
> `[Unreleased]` 段（1.4.0 之后又 67 个提交）**不计入本轮**，见第四节末尾。

**状态标记**：🔧 应修/应移植　⚠️ 需对照 pi 逐项核对（口径可能已有偏差）　✅ 已满足/无此问题（附理由）
❌ 结构性不适用（prux 无对应实现，无可改）　🆕 本区间新增/变更的候选移植项　🚫 决定不实现

---

## 零、总体判断

| 分类 | 条目 | 数量 |
|---|---|---|
| 🔧 应修（provider/工具/会话正确性） | 1.1–1.13 | 13 |
| ⚠️ 需核对（终端与 UI 生命周期） | 1.14、1.15 | 2 |
| ✅ 已满足 / 不适用 | 1.16 | 5 小项 |
| 🆕 新增能力（候选移植） | 2.1–2.5 | 5 |
| 数据面（`make sync-models`） | 三节 6 项 | — |
| ❌ 结构性不适用 | 第四节整表 + 第五节 | — |

**建议实施顺序**（按性价比）：

1. **P0 重试链（1.3 + 1.4 + 1.5）**：全在 `agent_session.rs::is_retryable_assistant_error` 一处，
   改动面最小、用户可见收益最大（gateway/代理故障不再一次就失败，请求 id 里的数字不再误重试）。
2. **P1 压缩输出上限（1.1 + 1.2）**：reasoning 模型上压缩/分支摘要直接失败，两处各改一行。
3. **P2 工具与协议正确性**：1.6（空白工具输出）、1.7（413 不算溢出）、1.9（`rg --no-config`）。
4. **P3 平台面**：1.10（PowerShell 探测）、1.8（mcp.json BOM）、1.13（wl-copy 进程组）、1.14（终端还原）。
5. **P4 会话一致性**：1.11（会话恢复的扩展工具，需引入 `defaultActive` 语义）。
6. **P5 新增能力**：2.1/2.2（分类器图片，彼此耦合）、2.3（客户端身份）、2.4（Rex）。
7. **数据面**：先 `make sync-models` 再动第三节的默认模型表。

---

## 一、Bug 修复（prux 已有功能，升级时应评估移植）

### 1.1 🔧 压缩摘要的输出上限由 `reserveTokens` 推出，reasoning 模型上直接失败

**pi 1.4.0 Fixed**（#9512 / #9075）：摘要请求原先吃固定输出上限，reasoning 把额度耗尽后
报 `generation hit the token cap`；现在摘要改用**模型的输出上限**，`compaction.reserveTokens`
只决定「何时开始压缩」。

**prux 现状**：`src/core/agent_session.rs:1550-1553`

```rust
let reserve = settings.reserve_tokens.max(1);
let budget = ((reserve as f64) * 0.8).floor() as u32;
let max_tokens = limit_model.max_tokens.unwrap_or(u32::MAX).min(budget);
```

默认 `reserveTokens = 16384` → 摘要上限 13107；用户把它调小（例如 2000）时上限只剩 1600，
thinking 一开就截断。`engine.rs::should_compact` 已正确只用 `reserve_tokens` 判触发点，
所以改这里不会影响触发阈值。

**建议**：`max_tokens` 取 `limit_model.max_tokens`（缺省时不传 `Some`，交给 provider 默认），
`reserve_tokens` 从摘要路径彻底移除；`generateSummary()` 侧同名参数一并废弃（pi 就是这么干的）。

### 1.2 🔧 分支摘要输出上限硬顶 4096

**pi 1.4.0 Changed**：`generateSummaryWithUsage()` 忽略 `reserveTokens`，分支摘要同 1.1 改用模型输出上限。

**prux 现状**：`src/core/agent_session.rs:4509-4521` `branch_summary_max_tokens()` 是
`min(model.max_tokens, 4096)`，调用点在 `:1874`。长 reasoning 的模型（gpt-6.x / opus 系）
在 4096 内被 thinking 吃光，摘要为空 → `on_compact_failure`。

**建议**：与 1.1 同源，直接取模型输出上限；补一条 `branch_summary_max_tokens` 用例改口径。

### 1.3 🔧 重试表缺 5 条 gateway / 代理 / OAuth 刷新串

**pi 1.4.0 Fixed**（#10920 / #11057 / #10996）：以下文案此前不重试，现在都按瞬时错误重试：

| pi 新增串 | 触发场景 |
|---|---|
| `upstream.?request.?failed` | 网关 `upstream_error: Upstream request failed` |
| `upstream_stream_read_error` | 网关上游流读失败 |
| `http/?2 stream failed` | `Upstream HTTP/2 stream failed` |
| `stream disconnected before completion` | 包着 Codex CLI 的代理中途断流 |
| `request to \S+ failed, reason:` | Vertex OAuth token 刷新瞬时失败（reason 常为空） |

**prux 现状**（实施后核对）：`src/core/agent_session.rs` 的 `RETRYABLE` 原是**纯子串**表，
其中 `"upstream request failed"` 实际已被 `"request failed"` 覆盖；剩余四条 `"http2"` /
`"ended without"` / `"stream ended before …"` 都匹配不到新增文案（`http/2` ≠ `http2`）。

### 1.4 🔧 状态码按纯子串匹配 → 请求 id / 字节数里的数字被当成可重试

**pi 1.4.0 Fixed**（#11057）：`429|500|502|503|504|520|524` 原先按裸子串匹配，
`req_011CU503xyz`、`5242880 bytes` 都会命中；现在要求前后不是字母或数字：
`(?<![a-z0-9])(?:429|500|502|503|504|520|524)(?![a-z0-9])`。

**prux 现状**：`agent_session.rs:2373-2380` 就是裸串 `"429"` … `"524"`，且整表 `to_lowercase()`
后 `contains`。所以 `req_011cu503xyz` 命中 `503`、`5242880 bytes` 命中 `524` → 对所有错误退避重试。
Rust 侧没有负向后顾（`regex` crate 不支持 lookaround），得改成「前后不是 ASCII 字母数字」的
手写判定，或直接对状态码用 `split` 边界判断。

**建议**：把状态码从 `RETRYABLE` 里摘出来单独判：`msg` 分段（非字母数字为界）后
与白名单精确比对；`RETRYABLE` 只留文字串。

### 1.5 🔧 ChatGPT Business 工作区上限未列入不可重试

**pi 1.4.0 Fixed**：`subscription_sharing_workplace_individual_usage_limit_exceeded`
（Business 工作区管理员设的用量上限）不重试，并提示找管理员提额。

**prux 现状**：`NON_RETRYABLE`（`agent_session.rs:2342-2354`）只有
`subscription_sharing_usage_limit_exceeded`。Business 那条既不命中它，文案里若带 `429`
还会命中 1.4 的状态码子串 → 对着不可恢复的额度上限反复退避重试。

**建议**：`NON_RETRYABLE` 补该串（放在 `subscription_sharing_usage_limit_exceeded` 之前也无所谓，
两条互不包含）；错误提示补一句「找工作区管理员提额」。

### 1.6 🔧 只有空白的工具输出被原样下发（OpenAI Completions / Responses / Google 400）

**pi 1.4.0 Fixed**（#8720）：bash 输出一个裸 `\r\n` 时，三个 provider 会 400；
现在只有空白的输出与「空输出」共用同一个占位符。

**prux 现状**：
- `src/core/provider/completions.rs:327-334`：`has_text = !text.is_empty()`，空白算有内容 → 原样发送；
- `src/core/provider/responses.rs:110-118`：同上，`"(no tool output)"` 只兜真空串；
- `src/core/provider/google.rs:381-389`：**连空输出都没有占位符**（`String::new()`），
  比 pi 1.3.0 还旧一步；只有图片时才是 `(see attached image)`。

**建议**：三处把判据从 `is_empty()` 改成 `trim().is_empty()`；Google 补
`"(no tool output)"` 与 `"(see attached image)"` 同一套口径。注意别把 `"(no tool output)"`
自己 trim 掉——它非空白，安全。

### 1.7 🔧 HTTP 413 不计为上下文溢出（Copilot 大请求体一次就死）

**pi 1.4.0 Fixed**（#11016）：任何 HTTP 413 都算上下文溢出，让调用方还能压缩后重试。

**prux 现状**：`agent_session.rs:3214-3227` `is_bodyless_status_error()` 只在
`provider == "cerebras"` 且「400/413 + 空 body」时算溢出（调用点在 `:3251`）；
其它 provider 的 413 走普通错误 → 会话卡死在超大请求上。

**建议**：`is_overflow_error_str` 之外再加一条：`provider returned 413 …` 无条件算溢出
（Cerebras 的 400 空 body 保留）。补 `context_overflow_covers_error_silent_and_length_stop` 用例。

### 1.8 🔧 `mcp.json` 带 UTF-8 BOM 时整份跳过

**pi 1.4.0 Fixed**（#11191）：`mcp.json` 以 BOM 开头时解析失败，所有 MCP server 被跳过，
`pi mcp add` / `remove` 也跟着失败。

**prux 现状**：`src/extensions/mcp/config.rs:881-892` `read_json_value()` 直接
`strip_json_comments(&text)` → `serde_json::from_str`，**没有剥 BOM**
（`settings.json` / `auth.json` / 技能 frontmatter 都过 `utils::mime::strip_bom`，只有这里漏了）。

**建议**：`read_json_value` 入口加 `mime::strip_bom`；补一条以 `\u{feff}{` 开头的解析用例。

### 1.9 🔧 `grep` 工具受 `RIPGREP_CONFIG_PATH` 影响，搜索语义被外部配置改掉

**pi 1.4.0 Fixed**（#11203）：用户 shell 里设了 `RIPGREP_CONFIG_PATH`（例如带 `--fixed-strings`）时，
`grep` 的正则搜索被静默改成字面量；现在固定 `rg --no-config`。

**prux 现状**：`src/core/tools/grep.rs:79-84` 的参数表从 `--json --line-number --color=never --hidden`
开始，**没有 `--no-config`** → 同一个 bug。

**建议**：参数表首位加 `--no-config`。

### 1.10 🔧 PowerShell 只在 `PATH` 里找，装到默认目录就找不到

**pi 1.4.0 Fixed**（#9501）：`pwsh` 不在 `PATH` 时回落
`%ProgramFiles%\PowerShell\7\pwsh.exe`，`powershell.exe` 回落
`%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe`。

**prux 现状**：`src/core/tools/powershell.rs:22-39` `find_powershell()` 只遍历 `PATH`、只认
`is_file()`，找不到就 `No PowerShell executable found` → Windows 上「装了但没加 PATH」直接不可用。

**建议**：按 pi 的顺序补两级回落（`ProgramFiles` → `SystemRoot`）。
顺带：pi 另一条 #9504（Store 别名被 `existsSync` 的 stat 拦掉）在 Rust 侧对应
`Path::is_file()`，对 0 字节 reparse point 仍返回 true，**大概率不复现**，改完跑一次实测即可。

### 1.11 🔧 恢复会话 / 切分支不激活「后来才注册」的扩展工具

**pi 1.4.0 Fixed**（#11062 / #11075）：恢复会话与 `/tree` 只装回分支记录的 loadout，
会话开始后才注册的扩展工具永远是非激活态；现在「分支既没加过也没删过的已注册工具」
默认激活，除非工具声明 `defaultActive: false`（整个分支都会搜，压缩前删掉的仍保持关闭）。

**prux 现状**：`src/core/agent_session.rs:1097-1117` `restore_session_tools()` 直接
`ToolSelection::Default(recorded)`，没有任何「未记录的已注册工具」补偿；
`ExtensionTool`（`core/extensions.rs:422`）也没有 `default_active` 字段。

**建议**：恢复时按「记录集合 ∪ 已注册且未记录过的扩展工具」求并集，
并给 `ExtensionTool` 加 `default_active: bool`（缺省 true，`false` 时不并入）。
需要在会话记录里区分「被用户移除过」与「当时还没注册」，prux 目前只有
`last_active_tools()` 快照，得改成整条分支扫描后取最后一次增删。

### 1.12 🔧 HTML 导出的 CJK 粗体/斜体渲染成字面星号

**pi 1.4.0 Fixed**（#3353 / #10154）：导出模板的 marked 不认「CJK 标点紧邻 `**`」的强调，
`**「引用」**` 会原样显示星号；pi 引入 `marked-cjk-friendly` 插件修好。

**prux 现状**：`assets/export-html/template.js:1593-1665` 用 marked 18，
自定义 tokenizer 只管 `html` / `tag` / `del`，`renderer` 只管 link / image / code / codespan；
`assets/export-html/vendor/` 只有 `marked.min.js` 与 `highlight.min.js`，**没有 cjk-friendly**。
TUI 侧走 pulldown-cmark（CommonMark 强调规则），不受影响。

**建议**：把 `marked-cjk-friendly` 的最小构建放进 `vendor/`，在 `marked.use({...})` 之前
`marked.use(cjkFriendlyExtension)`；`src/core/export_html.rs` 的内嵌清单与
`template.html` 的 `<script>` 位同步加一行。

### 1.13 🔧 `wl-copy` 与 prux 同进程组，Ctrl+Z 挂起后剪贴板守护进程一起停

**pi 1.4.0 Fixed**（#11179）：Linux 上复制后 `Ctrl+Z` 挂起，再粘贴会卡住——
因为持有剪贴板所有权的是 prux 进程组里的子进程，被一起 SIGTSTP 了；现在在 POSIX 上
`spawn(..., { detached: true })` 让它自成进程组。

**prux 现状**：`src/utils/clipboard.rs:241-253` `copy_to_wayland_clipboard()` 用裸
`std::process::Command::new("wl-copy").spawn()`，子进程留在 prux 的进程组里 → 同一个坑。
（X11 走 arboard 进程内实现，无外部子进程，不受影响；Termux 的 `termux-clipboard-set`
是短命进程，但也值得一并留意。）

**建议**：spawn 前 `CommandExt::process_group(0)`（`std::os::unix::process::CommandExt`，
Rust 1.64+），或复用 `utils::process::spawn_detached` 的进程组逻辑。
补一条断言子进程 pgid ≠ 自身 pgid 的 Linux-only 用例。

### 1.14 ⚠️ 异常退出路径没有还原终端（鼠标跟踪 / Kitty 键盘 / 备用屏残留）

**pi 1.4.0 Fixed**（#11170）：进程没走正常 teardown 就退出时，鼠标跟踪、焦点上报、
Kitty 键盘协议、备用屏全部留在开启态，shell 提示符被转义序列糊满；
现在注册进程 `exit` 钩子同步还原。

**prux 现状**：`src/modes/interactive.rs:1379-1412` `teardown_terminal()` 覆盖了
raw mode / 备用屏 / 括号粘贴 / 键盘增强 / 鼠标全量逆序列，但只在正常返回路径调用；
`src/core/crash_log.rs:199-217` 的 panic 钩子只记崩溃日志、**还原原钩子后直接放行**，
panic 展开出来的路径不保证跑到 teardown。SIGTERM / SIGHUP 走 `SHUTDOWN_REQUESTED` 标志
（`:99-120`）能优雅退出，Ctrl-C 在 TUI 里是普通按键，这两条没问题。

**建议**：panic 钩子内（`previous(info)` 之前）加一次「静默还原」：
`disable_raw_mode` + 备用屏/鼠标/键盘增强的逆序列直写 `stdout`。别在钩子里抢锁。
风险点：钩子里写 stdout 可能与正常 teardown 重复发序列，逆序列是幂等的，可接受。

### 1.15 ⚠️ 并发打开覆盖层时，前一个被顶掉（不是栈）

**pi 1.4.0 Fixed**（#11166 / #9909）：两个扩展同时弹 `select`/`confirm`/`input`/`editor` 时，
第二个替换第一个、第一个的 promise 永远不 settle；现在关掉可见的那个会露出前一个。

**prux 现状**：`src/modes/interactive/overlay.rs:181-190` `open()` 是
「换主 + 给旧覆盖层发 `Closed`」——旧的那个**不会挂死**（pi 的核心 bug 不复现），
但也不会回来，用户看到的是「前一个对话框凭空消失」。
prux 的扩展 UI 是 `overlay_view(id)` / `on_overlay_event(id, ev)` + `app.overlay: Option<OverlayState>`
（`app.rs:601`），单槽位。

**建议**：低优先。若要对齐，把 `Option<OverlayState>` 改成 `Vec<OverlayState>` 栈，
`open()` push、`close()` pop 并重新同步上一个；改动波及 `render/overlay.rs` 的
滚动状态与 `handlers/overlay.rs` 的按键路由，收益仅限「两个扩展同时要输入」。

### 1.16 ✅ 已满足 / 不适用的小项

| pi 条目 | prux 结论 |
|---|---|
| `@` 文件补全对 `[` / `(` 等正则元字符无结果（#10972） | ✅ 不适用：prux 的 `@` 补全走 `utils::fuzzy::fuzzy_filter`（子序列打分），全程无正则（`handlers/suggest.rs:451`、`:565`） |
| 未闭合 `$$` 吞掉后面所有 Markdown 表格（#11159） | ✅ 不适用：TUI 用 pulldown-cmark 的 `ENABLE_MATH`，math 是行内事件、跨不过块边界，且已有 `unclosed_math_shows_literal` 用例（`render/markdown.rs:1622`）；导出模板没开 math |
| 输出上限截断的 tool call 被丢弃、tool result 留下（#11227） | ✅ 已满足：`agent_loop.rs:713-720` + `:827-880` 为每个截断的调用**补一条错误 toolResult**，调用与结果成对，不会留下孤儿结果 |
| `/compact <instructions>` 在被切开的轮次上被忽略（#11058） | ✅ 不适用：prux 的切点只落在完整消息上（`compaction/engine.rs::find_compaction_cut_index`，toolResult 不作切点），不存在「切开一轮」 |
| OpenAI Responses 延迟工具加载丢 prompt cache（#10940） | ❌ 不适用：prux 没有 `additional_tools` 式延迟工具声明（`content_block::deferred` 是**工具执行**推迟，不是工具清单重写） |

---

## 二、本区间新增 / 变更功能（候选移植）

### 2.1 🆕 分类器图片输入（TypeSafe Jev / Cloudflare Clef）

**pi 1.4.0 Added**：`models.classify()` 的 `images` 现支持 TypeSafe 的 Jev（`typesafe`）、
Cloudflare 的 Clef（`cloudflare-workers-ai`），以及目录声明了图片输入的 OpenRouter 决策模型。
三种线上形态各不相同：TypeSafe 走 `assets`、Clef 走 `images` 的 data URL、OpenRouter 走
`image_url` parts。另外新增 `clef-omni`、`jev-1.14.0` 两个分类器模型条目。

**prux 现状**：
- 协议层已有 `ClassifierContext.images`（`core/provider/classifier.rs:70-81`），但唯一的
  `typesafe-system-one` 实现**明确拒绝图片**（`:168-176`）；
- OpenRouter 决策模型侧已支持（`core/provider/decisions.rs:169` 的 `image_url` data URL）；
- 目录里没有 `clef-omni` / `jev-1.14.0`。

**建议**：`classify_typesafe_system_one` 按 pi 的 `assets` 形态拼图片、去掉拒绝分支；
新增 Clef 的 `images` data URL 形态（可挂在同一个 `system-one` 协议实现上按模型/服务分流）；
目录侧随 `make sync-models` 一起进来。测试补「同一份 `images` 在三种形态下的请求体」。

### 2.2 🆕 分类器的 `compat`：`SystemOneCompat`（`imageInput` / `sendImageOptInHeader`）

**pi 1.4.0 Added**：分类器模型也有 `compat`（`SystemOneCompat`），内置条目显式声明；
没有声明的按 provider / `baseUrl` / 模型 id 推断。`imageInput` 决定愿不愿意发图片，
`sendImageOptInHeader` 决定要不要发实验性 opt-in 头（TypeSafe 的
`TypeSafe-Danger-Zone-Images`）。同时 **provider 级 `compat` 现在也作用于分类器条目**。

**prux 现状**：`core/model_resolver.rs:199-220` 的注释与实现都写着
「`compat` 只作用于 chat（非 chat 条目只有 baseUrl 变）」；分类器请求头只有
`provider_extra_headers(model)`（`classifier.rs:192`），没有 opt-in 头。

**建议**：先放宽 `model_resolver` 的 compat 合并范围到 `classifier` 条目（image 条目维持不变），
再在 `classifier.rs` 读 `compat.imageInput` / `compat.sendImageOptInHeader`。
这是 2.1 的前置——没有它，「哪些服务吃图片」只能靠模型 id 硬编码。

### 2.3 🆕 客户端身份：`User-Agent` + provider attribution 头

**pi 1.4.0 Added/Breaking**（`packages/ai` + `packages/agent`）：新增
`identity` 请求选项与 `createModels({ identity })` 默认值，统一决定
`User-Agent`（`"detailed"` 带版本 / `"short"` / 自定义）以及
OpenRouter / NVIDIA NIM / OpenCode 的归因头（`attribution: { name, url, categories? }`）。
默认 UA 从 `pi (<platform> <release>; <arch>)` 改成 `pi/<version> (<platform>; <runtime>; <arch>)`，
`getPiUserAgent()` 移除、改用 `formatUserAgent()`。扩展的 `before_provider_headers`
不再看到这些归因头（返回 `null` 仍可删）。

**prux 现状**：模型请求**完全不发 `User-Agent`**，也没有 OpenRouter 的
`HTTP-Referer` / `X-Title` 归因头；`provider_extra_headers`（`core/provider.rs:766-815`）
只拼目录头、会话亲和头、opencode 会话头、`models.json` 覆盖与鉴权。
opencode 侧已有 `x-opencode-session` / `x-opencode-client`（`:697-698`），归因头缺失。
prux 也没有 `before_provider_headers` 这类扩展钩子，所以「钩子看不到归因头」这条无处置。

**建议**：在 `provider_extra_headers` 里补 `User-Agent: prux/<version> (<os>; <arch>)`
与 OpenRouter / NVIDIA 的归因头，取值集中到一处常量/配置，别散落。是否给用户留配置入口
（`identity`）按需求再定——prux 目前没有任何「上报身份」的开关。

### 2.4 🆕 Rex 终端支持（OSC 8 超链接、truecolor、内联图片）

**pi 1.4.0 Added**（#11131 / #11150 / #11183）：识别 `TERM_PROGRAM=rex`，给 OSC 8 超链接、
truecolor 和 Kitty 图片；此前超链接退化成 `text (url)`。

**prux 现状**：`utils/terminal_image.rs:325-355` `env_protocol()` 的白名单只有
kitty / ghostty / wezterm / warp / iTerm2；`utils/terminal_caps.rs:206-215` 决定
超链接与配色能力。Rex 落到「未知终端」→ halfblocks 图片 + `text (url)`。
（这正是上一轮 3.11 里 Herdr 的同一类缺口。）

**建议**：`env_protocol()` 的 kitty 分支加 `TERM_PROGRAM == "rex"`；
`terminal_caps` 的超链接/truecolor 判定同样加一条。若能拿到 Rex 的真实环境变量，
一并补测试；纯环境变量判定，成本极低。

### 2.5 🆕 `models.json` 字段参考文档（可选）

**pi 1.4.0 Added**（#10790）：新增由 `models.schema.json` 生成的
`docs/models-configuration.md`，列全 provider / model / model override / compat 各字段，
并修了 schema 缺 description 导致编辑器没有提示的问题。

**prux 现状**：文档是手写的、按主题拆开：`assets/docs/models.md`（Provider 字段 / Model 字段，
`:107-145`）+ `assets/docs/custom-provider.md`（`compat` 字段）。**仓库里没有
`models.schema.json` 这个真源**，`$schema` 指向的是外部 URL。

**建议**：低成本选项是**只补文档**：把 `compat` 各字段汇总成一张表并互链；
高成本选项是引入 `schemas/models.schema.json` + 生成器。手写文档目前已覆盖主要字段，
差距在 compat 明细，建议先做前者。

---

## 三、模型目录 / 数据同步（与代码改动解耦）

1. ⚠️ **`scripts/sync-models.py:36` 的 `SYNC_MARK` 仍指 `ROOT/sync.md`，而该文件已移到
   `migrations/sync.md`**（提交 `fabf1cf`）。当前记录值是 `1.3.0`，所以脚本的
   「sync.md 记录 X，pi-ai 为 Y」提示会**静默失效**。动数据面前先修这一行并改成 `1.4.0`。
2. `assets/models/**` 需按 pi 1.4.0 重新同步（`make sync-models`，校验用 `make sync-models-check`）：
   - `MiniMax-M3.1-Flash` 进 `minimax` / `minimax-cn`，thinking low–max 走 `output_config.effort`，
     且**不能关闭 thinking**；新 MiniMax 模型不再需要手工白名单（只留 M2 / M2.1 / M2.5 / M2.5-highspeed 隐藏）；
   - `xai` 的 Grok 4.3 / 4.5 增加 `xhigh` 采样档（现有目录里两者都没有 `xhigh` 映射）；
   - Cloudflare Workers AI 分类器目录修正（Clef Flash $0.038、Jev $0.042 不再是免费），
     新增 `@cf/cloudflare/clef-omni`、`typesafe/jev-1.14.0`；
   - `openai-codex` 移除 `gpt-5.5` / `gpt-5.3-codex-spark` —— ❌ prux 的
     `SUPPORTED_PROVIDERS` 无该 provider，无需处理。
3. **默认模型表**（`src/core/model_resolver.rs:1312-1345` `default_model_id`）需同步三处：

   | provider | prux 现值 | pi 1.4.0 |
   |---|---|---|
   | `github-copilot` | `gpt-5.4`（上游已退役） | `gpt-6.1-sol` |
   | `nvidia` | `nvidia/nemotron-3-ultra-550b-a55b` | `nvidia/nemotron-3.5-super-vl-120b-a12b` |
   | `xiaomi` | `mimo-v2.5-pro`（已出目录） | `mimo-v2.6-pro` |

   `model_resolver.rs:2165-2166` 有「`openai` 目录应保留 `gpt-5.4`」的断言，那是 `openai`
   provider，**不要跟着改**；要改的是 `github-copilot` / `azure` 的默认值与
   `openai` 目录里 `gpt-5.4` 是否仍存在。
4. `compaction.modelOverrides` 已存在（`settings_manager.rs:926-967`、`engine.rs::for_model`），
   本轮无需改动。
5. `migrations/sync.md` 改成 `1.4.0`；`assets/changelog/v.1.4.0.md` 已经在写（含 prux 自有
   `ask-user-question` / `ponytail` / `remote_default` 等条目），发版前把本轮用户可见改动补进去。

---

## 四、行为 / 接口变更（对齐时容易踩的坑）

| # | 变更 | prux 影响 |
|---|---|---|
| 4.1 | `packages/ai`：请求选项 `telemetryContext` → `telemetry?: TelemetrySpan`（无兼容别名）；新增 `AI_TELEMETRY_SCHEMA` 与 `pi.ai.request` / `pi.ai.poll` 追踪 | ❌ 不适用：prux 无 telemetry 层 |
| 4.2 | `packages/ai`：默认 UA 格式变更、`getPiUserAgent()` 移除、Bedrock/图片/分类器请求也发 UA | 见 2.3（prux 目前一个 UA 都不发） |
| 4.3 | `packages/ai`：`resolveTranscriptTools()` 改吃 `TranscriptToolSupport` 枚举、`anchorsAdditions` → `foldedSystemMessages` | ❌ 不适用：prux 无 transcript tool 折叠模型 |
| 4.4 | `packages/agent`：`AgentOptions.identity` / `Agent.identity` | ❌ 不适用（并入 2.3） |
| 4.5 | `packages/{chord,durable}`：`Context.telemetry` 必填、SQLite 接口搬到 `chord/sqlite`、durable 的 `sleep()` → `sleepUntil()` / `setTimer()` / `sleeping` 状态 / schema v2 / 崩溃循环护栏 / `settings.afterFailedRun` / `checkpointWhen` 必填 | ❌ 不适用：prux 无宿主层与持久任务图（扩展的 `durable` 能力由 `extensions/tasks` 自己实现，语义不同） |
| 4.6 | `packages/telemetry`：链路 / 指标 / 传播整套重做（`addLink()`、`TelemetryMetrics`、`StoredSpan`、`renderTrace()`） | ❌ 不适用：prux 无 telemetry |
| 4.7 | `packages/coding-agent`：`generateSummary()` / `generateSummaryWithUsage()` 忽略 `reserveTokens` | 见 1.1 / 1.2 |
| 4.8 | 扩展的 `before_provider_headers` 不再看到归因头 | ❌ 不适用：prux 无该钩子 |
| 4.9 | `packages/codemode` / `env` / `mcp` 在 1.4.0 段**没有条目** | — |
| 4.10 | `ToolControl.terminate` 由 `true` 放宽为 `boolean`（durable） | ❌ 不适用 |

**未收录**：`packages/ai` / `coding-agent` 的 `[Unreleased]` 段（1.4.0 之后 67 个提交），
含 `StreamingJsonParser` 重写（流式工具参数从 O(n²) 降到 O(n)，`ToolCallArgumentsStream` 的
`JsonChange`）、grammar 工具输入增量、`/model` 与 `/resume` 的搜索排序、HTML 导出上万条目白屏、
设备码登录按键即开浏览器并复制验证码、Kimi K3.1 Pro 请求参数、Claude 中途 system 消息 400、
`pi update --min-release-age`、Bun `node --watch` 桥接等。下一轮（1.4.0 → 下一版）再评估。

---

## 五、整段不列出的 pi 功能（prux 无对应实现）

以下在 changelog 里有条目，但 prux 没有对应功能，按收录规则不逐条展开：

- **`packages/{chord,durable,telemetry}`**：宿主运行时、SQLite 接口、链路/指标/传播——
  Rust 单进程单 crate，无插件宿主模型。
- **`packages/env`**：环境 daemon 与 `listFiles` / pty。
- **`packages/codemode`**：`CodemodeSandbox` 的 worker 池与 `PI_CODEMODE_MAX_WORKERS`
  （pi 1.4.0 的「等空闲 worker 而不是立刻失败」）。prux 的 codemode 是进程内 QuickJS，
  没有 worker 概念；`models.*` 的并发是 `Semaphore(4)` 排队（`extensions/codemode.rs:58`、`:1019`），
  **已经是排队语义**，不需要移植。
- **Node / npm / Nix 打包面**：`managed-install.json` 的 BOM、`pi update` 的
  `--min-release-age`、install-lock、Node 版本约束。
- **无对应 provider**：`openai-codex`（含 1.4.0 移除的两个模型）、Amazon Bedrock、
  llama.cpp、Radius 网关。
- **pi-ai / pi-tui 的库 API**：`StreamingJsonParser`、`ToolCallArgumentsStream`、
  `formatUserAgent()`、`getIdentityHeaders()`、`mergeClientIdentity()` 等只服务 TS 调用方。

---

## 六、实施记录（bug 修复 + 终端/导出，已完成）

本轮只做第一节的 🔧 与 ⚠️；1.15（覆盖层改栈）与二、三节的新增能力、数据面**未开工**。

| # | 项 | 改动位置 | 关键测试 |
|---|---|---|---|
| 1.1 | 压缩摘要用模型输出上限 | `agent_session.rs::maybe_compact_inner`（删掉 `reserve_tokens * 0.8`） | 现有压缩用例 |
| 1.2 | 分支摘要去掉 4096 硬顶 | `agent_session.rs::branch_summary_max_tokens` 改返回 `Option<u32>` | `branch_summary_max_tokens_uses_the_model_output_limit` |
| 1.3 | 重试表补网关/代理/OAuth 串 | `agent_session.rs`：`RETRYABLE` 加 4 条 + `is_oauth_token_refresh_failure` | `retryable_error_classification_matches_pi` |
| 1.4 | 状态码边界匹配 | `agent_session.rs::contains_http_status_code`（状态码移出 `RETRYABLE`） | 同上（`req_011CU503xyz`、`5242880 bytes`） |
| 1.5 | Business 工作区上限不重试 | `agent_session.rs::NON_RETRYABLE` | 同上 |
| 1.6 | 空白工具输出用占位符 | `completions.rs` / `responses.rs` 判据改 `trim()`；`google.rs` 补 `"(no tool output)"` | `whitespace_only_tool_output_uses_the_empty_placeholder`、`empty_and_whitespace_tool_outputs_use_the_placeholder` |
| 1.7 | 任何 413 算溢出 | `agent_session.rs::is_payload_too_large` | `context_overflow_covers_error_silent_and_length_stop` |
| 1.8 | `mcp.json` 剥 BOM | `extensions/mcp/config.rs::read_json_value` | `config_reads_through_a_utf8_bom` |
| 1.9 | `grep` 加 `--no-config` | `core/tools/grep.rs` | 现有 grep 用例 |
| 1.10 | PowerShell 两级回落 | `core/tools/powershell.rs::find_powershell` | 仅 Windows 可实测 |
| 1.11 | 会话恢复激活后注册的扩展工具 | `session_manager.rs::ever_active_tools` + `agent_session.rs::restore_session_tools` | `restore_session_tools_restores_recorded_tools` |
| 1.12 | HTML 导出 CJK 强调 | `assets/export-html/vendor/marked-cjk-friendly.min.js`（新增，MIT）、`template.html`、`template.js`、`core/export_html/export.rs` | 手工导出验证 |
| 1.13 | `wl-copy` 自成进程组 | `utils/clipboard.rs`：`process_group(0)` | 仅 Linux/Wayland 可实测 |
| 1.14 | panic 还原终端 | `modes/interactive.rs`：`restore_terminal_modes` + `install_panic_terminal_restore` | 手工验证（`cargo run` 后人为 panic） |

**行为口径变化（升级后用户可见）**：

- 重试：网关/代理/OAuth 瞬时故障会退避重试；请求 id、字节数里的数字不再触发重试；
  ChatGPT Business 的工作区上限直接失败。
- 压缩：摘要改用模型输出上限，`compaction.reserveTokens` 只影响触发时机（结论：把
  `reserveTokens` 调小不再连带把摘要变短）。
- 工具输出：只有空白的输出在三个 provider 上一律变成占位文案（Google 的空输出也从 `""` 改成占位文案）。
- 溢出：任何 413 都会触发压缩后重试，而不是当作普通错误。
- 会话：恢复会话/切分支会激活「分支没增删过」的已注册扩展工具；被用户移除过的仍保持关闭。
- 终端：panic 退出会自行关掉 raw mode / 鼠标 / 键盘增强 / 备用屏。
- 导出：CJK 标点旁的粗体斜体正常渲染；导出文件体积增加约 5 KB（内嵌插件）。
- `mcp.json`：BOM 不再导致整份配置被跳过。

**未做**：1.15（覆盖层改栈，改动波及滚动状态与按键路由，收益仅限“两个扩展同时要输入”）；
2.1–2.5 新增能力；第三节数据面同步。

---

## 七、落地检查清单

**P0 重试链**
- [x] 1.3 `RETRYABLE` 补 4 条 gateway/代理串 + Vertex OAuth 刷新前缀匹配
- [x] 1.4 状态码改为边界匹配，不再裸子串
- [x] 1.5 `NON_RETRYABLE` 补 ChatGPT Business 工作区上限
- [ ] `retryable_error_classification_matches_pi` 补全部断言

**P1 压缩**
- [x] 1.1 压缩摘要改用模型输出上限，`reserveTokens` 只管触发
- [x] 1.2 `branch_summary_max_tokens` 去掉 4096 硬顶

**P2 工具与协议**
- [x] 1.6 空白工具输出按空输出处理（completions / responses / google 三处，google 补占位符）
- [x] 1.7 任何 413 计为上下文溢出
- [x] 1.9 `grep` 加 `--no-config`

**P3 平台面**
- [x] 1.10 PowerShell 补 `ProgramFiles` / `SystemRoot` 回落
- [x] 1.8 `mcp.json` 剥 BOM
- [x] 1.13 `wl-copy` 自成进程组
- [x] 1.14 panic 钩子还原终端

**P4 会话一致性**
- [x] 1.11 恢复会话合并「未记录的已注册扩展工具」+ `ExtensionTool.default_active`

**P5 新能力（可选）**
- [ ] 2.2 分类器 compat（2.1 的前置）
- [ ] 2.1 分类器图片输入三种线上形态
- [ ] 2.3 `User-Agent` + 归因头
- [ ] 2.4 Rex 终端
- [ ] 2.5 `models.json` 字段参考文档（只补 compat 表）
- [x] 1.12 HTML 导出 CJK 强调（vendor `marked-cjk-friendly`）
- [ ] 1.15 覆盖层改栈（低优先，本轮未做）
- [ ] 第三节数据面：修 `scripts/sync-models.py` 的 `SYNC_MARK` → `make sync-models` →
      默认模型表三处 → `migrations/sync.md` 改 `1.4.0`
- [x] 收尾：`cargo fmt`、`cargo test`（3984 + 集成全绿）、`cargo clippy`、`assets/changelog/v.1.4.0.md` 补段
