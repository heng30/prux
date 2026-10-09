# Prux 迁移指南：pi 1.0.2 → 1.1.0

> **来源**：pi 仓库 tag `v1.0.2` → `v1.1.0` 之间各包 `packages/*/CHANGELOG.md` 的
> `[1.0.3]`、`[1.0.4]`、`[1.1.0]` 三个 release 段
> （`git diff v1.0.2..v1.1.0 -- 'packages/*/CHANGELOG.md'`），并用
> `git diff v1.0.2..v1.1.0 -- packages/...` 逐条核对源码改动。
> prux 基线：pi **1.0.2**（`Cargo.toml version = 1.0.2`，`assets/changelog/` 只到 `v.1.0.2.md`，
> 上一份指南见 `migrations/migration-v1.0.2.md`）。`sync.md` 里的 `1.1.0` 标记是**目标**，不是现状。
>
> **收录规则**（按用户要求）：只收录
> ①「prux 已实现功能的 bug 修复」与
> ②「1.0.2→1.1.0 区间内 pi 新增/变更的功能」。
> prux 没有、且在本区间之前就已存在的 pi 功能（Mistral / Bedrock / Radius / llama.cpp / `openai-codex` /
> faux provider / Node 打包与 `pi update` 更新器 / `packages/{durable,env,server,client,protocol,telemetry}`）
> **不逐条列出**，只在第五节汇总说明。

**状态标记**：🔧 应修/应移植　⚠️ 需对照 pi 源码逐项核对（行为可能已有偏差）　✅ 已满足/无此问题（附理由）
❌ 结构性不适用（仅备注）　🆕 本区间新增、prux 缺失的候选移植项

---

## 零、总体判断（先看这张表）

| 分类 | 条目 | 数量 |
|---|---|---|
| 🔧 必修 bug 修复 | 1.1、1.3、1.7、1.9、1.10、1.24 **已修复** | 6 |
| ⚠️ 需核对（行为已有偏差） | 1.2、1.5、1.6、1.17、1.18 **已修复**；1.29 待定（靠 rmcp 默认值） | 6 |
| ✅ 已满足 / 无此问题 | 1.11、1.12、1.13、1.14、1.16、1.19 | 6 |
| 🆕 本区间新增、prux 缺失 | 2.1–2.8、2.10–2.14、2.16–2.18、2.22 | 17 |
| ✅ 已满足（无需再动） | 2.9 | 1 |
| ❌ 结构性不适用 | 1.4（并入 2.12）、1.8、1.15、1.20–1.23、1.26–1.28、2.15、2.19、2.20 | — |

**最短落地路径（按性价比排序）**：

1. **重跑模型目录同步**（1.24 + 2.16 + 2.17 的数据面）——**已完成**（见第八节）：
   把 `SUPPORTED_PROVIDERS` 补上 `azure`，
   再 `make sync-models`，一次性拿到 Claude Haiku 5.5、GPT-6 Luna、azure 的 Foundry 条目，
   以及 OpenCode / OpenRouter / Vercel / Google / MiniMax 的 prompt-length 定价档。
2. **可重试错误表补两条**（1.1）：`server_busy`、`servers are currently busy`，一行级改动。
3. **`--tools` 三件套**（2.1 通配 / 2.2 MCP 保留 + `--no-mcp` / 2.3 `+name`-`name`）：
   改动集中在 `args.rs` + `ToolSelection`，但影响面广，建议一起做。
4. **codemode 输出与图片**（2.4–2.8）：四条互相独立的小改动。
5. **`agent_settled.aborted` + 工具 `durationMs`**（1.5 / 2.10 / 2.12）：事件载荷补齐，UI 侧顺带修 `Took`。
6. **MCP OAuth 可取消 + 15s 超时**（1.7 / 2.14）：改动最大，需要动登录状态机。
7. **OSC 7501 程序状态**（2.13）：纯新增，可最后做。

---

## 七、实施记录（bug 修复，已完成）

本轮先修了第一节里所有可落地的 bug；实现细节与验证如下。

| # | 项 | 改动位置 | 关键测试 |
|---|---|---|---|
| 1.1 | `server_busy` 可重试 | `core/agent_session.rs` `RETRYABLE` | `retryable_error_classification_matches_pi` |
| 1.2 | Anthropic 登录端口回落 | `core/oauth/anthropic.rs::start_authorize_flow` | `occupied_callback_port_falls_back_to_a_free_loopback_port` |
| 1.3 | OAuth 刷新不被取消 | `core/auth.rs::ensure_oauth_valid` 改独立任务 + `clear_oauth_refresh_lock` | `failed_oauth_refresh_releases_the_lock` |
| 1.5 | `Took` 只测工具本身 | `core/agent_session.rs` 顺序/并行两条路径（`push_tool_result_message` 改 `Option<u64>`） | `tool_duration_excludes_after_tool_call_hook`、`unknown_tool_reports_pi_not_found` |
| 1.6 | 技能提示不点名隐藏工具 | `core/system_prompt.rs`：`hidden_tools` + `SkillFileReader` + `declared_tool_names`；`compose_tools` 不再预过滤 | `skills_hint_names_read_when_declared` 等四条 |
| 1.7 | MCP OAuth 可取消 | `extensions/mcp/oauth.rs`：15s 超时 + `CancelAwareOAuthHttpClient` + `*_with_cancel`；`extensions/mcp.rs::spawn_login` 登记；`core/extensions/cancel.rs`（新增） | `receive_callback_aborts_on_cancel`、`idle_escape_cancels_background_work`、`extensions::cancel::tests::*` |
| 1.9 | 切会话重置文本选择 | `modes/interactive/app.rs::reset_text_selection` + 四个 transcript 重建入口 | `session_switch_resets_text_selection` |
| 1.10 | Termux 剪贴板 | `utils/clipboard.rs`：`is_termux` + `termux-clipboard-get/set` | `termux_detection_follows_the_env_var` |
| 1.17 | 终端消失安静退出 | `modes/interactive.rs`：`is_dead_terminal_error` / `is_dead_terminal_io`，错误构造改 `Error::Io` | `dead_terminal_errors_are_recognized` |

**新增的通用设施**：`core/extensions/cancel.rs` —— 后台可取消工作的登记处
（`register_background_cancel` / `cancel_background_work` / `background_work_count`）。
UI 在 Esc / Ctrl+C、会话关闭与退出时统一取消，不必知道是哪个扩展在跑后台活；
扩展侧只用「登记 + 拿 guard」。目前唯一使用者是 MCP OAuth 登录。

**行为口径变化（升级后用户可见）**：

- 1.5：未真正执行的工具调用（未知工具、被 block）的 `toolResult` **不再带 `durationMs`**
  （此前是 `0`，会渲染出 `Took 0.0s`）；真正执行过的调用其耗时不再包含 `after_tool_call` 钩子。
- 1.6：系统提示的 `SystemPromptOptions` 新增 `hidden_tools`；`selected_tools` 现在是
  **选中集**而不是「已扣除隐藏项」的集合（对外部调用方无影响：不传 `hidden_tools` 时两者相同）。
- 1.2：53692 被占用时**不再**退化为「粘贴 redirect URL」，而是继续走浏览器回调（换端口）。
- 1.7：MCP OAuth 对授权服务器的每个请求从 30s 收紧到 **15s**；`/mcp login` 在 Esc 下
  会回一条 `MCP OAuth sign-in for \`x\` was cancelled.` 的 Info 通知。

**验证**：`cargo test` 全绿（lib 3230 项 + 全部集成测试目标）；`cargo fmt --check` 通过；
`cargo clippy --lib --tests` 只剩改动前就存在的 3 条 warning
（`settings_manager` 1、`tasks/widget` 2）。

> 已知偶发（非本轮引入）：全量并行跑 lib 测试时
> `extensions::subagent::tests::cli_flag_workflow_file_launches_a_run` 等碰全局状态的用例
> 会偶发失败（单独跑必过），与 `migration-v1.0.2.md` 第八/十六节记录的同源。

**未做（属新增功能，不在本轮 bug 修复范围）**：2.10 的
`tool_execution_end.durationMs`、2.22（codemode 内联 guidelines）。
（1.24 模型目录同步已在第二轮完成，见第八节。）

---

## 一、Bug 修复（prux 已有功能，升级时应评估移植）

### 1.1 🔧 `server_busy` / `servers are currently busy` 不重试

**pi 1.1.0 Fixed**（ai #10543）：`RETRYABLE_PROVIDER_ERROR_PATTERN` 新增
`"server_busy"` 与 `"servers are currently busy"`，此前这类 provider 瞬时错误会直接结束回合。

**prux 现状**：`src/core/agent_session.rs:2245` 的 `RETRYABLE` 列表里**没有**这两个子串，
`NON_RETRYABLE`（同文件 2226 行起）也没有，因此命中的请求错误会被直接抛给用户。

**建议**：在 `RETRYABLE` 里补这两条（放在「服务端瞬时负载」分组）。补一条断言
`is_retryable_assistant_error("provider server_busy")` 为真的用例。

> **状态：已修复**。`RETRYABLE` 补了 `server_busy` / `servers are currently busy`；
> 断言见 `retryable_error_classification_matches_pi`。

---

### 1.2 ⚠️ Anthropic 浏览器登录端口 53692 被占用时回落到空闲 loopback 端口

**pi 1.0.2→1.1.0 修复**（ai #10571）：`CALLBACK_PORT = 53692` 绑定失败时，pi 改为
`startCallbackServer(0)` 让 OS 另取一个空闲端口，并把该端口写进 `redirect_uri`
（pi 注释称 Anthropic 接受任意 loopback 端口）；`redirectHost` 固定为 `localhost`。
真正的 `REDIRECT_URI` 常量只在连空闲端口都绑不上时用作粘贴回退的占位值。

**prux 现状**：`src/core/oauth/anthropic.rs:57 start_authorize_flow()` 在
`CallbackServer::bind(CALLBACK_PORT).ok()` 失败时**不换端口**，而是把 `server` 置 `None`、
退化为纯粘贴 redirect URL，且 `redirect_uri` 仍是固定的 `http://127.0.0.1:53692/callback`。
源码注释明确写着「redirect_uri 是注册在 Anthropic 侧的固定端口，换端口也收不到回调」——
这与 pi 1.1.0 的判断相反，需要核对哪边是对的（pi 1.1.0 的行为说明 Anthropic 侧不做端口白名单）。

**建议**：确认 pi 侧改动后对齐：绑定失败 → 再 `bind(0)`，用真实端口拼 `redirect_uri`，
失败才退化为粘贴模式。Windows 上 Hyper-V/WSL 的端口排除会命中这个场景。

> **状态：已修复**。`start_authorize_flow()` 改为 `bind(CALLBACK_PORT).or_else(|_| bind(0))`，
> 用真实端口拼 `redirect_uri`；两个端口都绑不上才退化为粘贴。断言见
> `occupied_callback_port_falls_back_to_a_free_loopback_port`。

---

### 1.3 🔧 OAuth 刷新在请求被取消/被替代时丢弃轮转后的 refresh token

**pi 1.0.3 Fixed + 1.1.0 重构**（ai #10565 前后）：新增 `refreshStoredOAuthCredential()`，
在凭据锁内做「再检查 → 刷新 → 持久化」，且**一旦刷新已开始就不再理会调用方的
`signal`**（只受 15s 超时约束），否则被取消的调用方会丢掉唯一的有效 refresh token。
`ModelsImpl.resolveProviderAuth` 与 `resolveStoredOAuth` 都改走这条路径。
1.0.3 同时修了「订阅登录在 token 刷新期间被取消后报 `refresh_token_invalidated`」。

**prux 现状**：`src/core/auth.rs:592 ensure_oauth_valid()` 直接 `.await` 刷新 future，
`write_oauth_credential` 只在成功路径（同文件 638 行起）写回。请求被取消 = future 被 drop
= 刷新结果与旋转后的 refresh token 一起丢失，正是 pi 两次修复针对的问题。
`src/core/oauth/openai_chatgpt.rs:157` 的 `refresh_token` 也没有 `refresh_token_invalidated` 处理。

**建议**（成本 M）：把刷新拆成「锁内同步段 + detached 完成段」：`tokio::spawn` 一个
不受调用方取消影响的刷新任务，调用方用 `select!` 等它（超时 15s），刷新结果一律持久化；
并发调用者通过锁内的「再检查」合并为一次刷新。补一条「取消后凭据仍是新的」用例。

> **状态：已修复**。刷新改在 `tokio::spawn` 的独立任务里跑：发起方被取消不中断它，
> 结果一律落盘，任务结束时无论成败都摘掉登记（旧实现 `?` 早退会永久锁死该 provider）。
> 断言见 `failed_oauth_refresh_releases_the_lock`。

---

### 1.4 ❌ `agent_settled` 事件缺少 `aborted`

见 2.12（属新增载荷，归到新功能一节处理）。

---

### 1.5 ⚠️ bash / PowerShell 的 `Took` 在 reload 后丢失，且实时 `Took` 混入 wall-clock 步骤

**pi 1.1.0 Fixed**（#10549）：工具结果现在记录 `durationMs`（单调时钟、只测 `execute()`、排除 hooks），
`Took` 优先用这个记录值；renderer 自己的 `Date.now()` 差值是实时进度与旧结果的回退。
relaod 会话后 `Took` 不再消失，实时值也不再包含 `after_tool_call` 等步骤。

**prux 现状**：`src/core/agent_session.rs:2871/2930` 已把 `duration_ms` 落到 toolResult 消息并持久化，
`src/core/provider.rs:571` 有字段。但续航点是：
① `duration_ms` 是**包裹计时**（`agent_session.rs:3931` 起，`started.elapsed()` 含钩子），
不是 `execute()` 自身的耗时；
② bash / powershell 工具各自算了 `wall_time_seconds`（`src/core/tools/bash.rs:474`、
`src/core/tools/powershell.rs:187`）却只塞进 structuredContent，渲染层的 `Took` 未取用它。

**建议**：把 `duration_ms` 的测量点收窄到 `tool.execute()` 调用本身；
`Took` 渲染优先取工具结果里的 `duration_ms`，取不到再回落到渲染层计时。

> **状态：已修复**。顺序/并行两条路径的 `duration_ms` 都在 `after_tool_call` 之前收口，
> 只测工具本身；未真正执行的调用为 `None`。断言见
> `tool_duration_excludes_after_tool_call_hook`、`unknown_tool_reports_pi_not_found`。

---

### 1.6 ⚠️ 系统提示的规则与技能提示点名被 `prepareLoadout` 隐藏的工具

**pi 1.0.4 Fixed**（#10343）：`BuildSystemPromptOptions` 新增 `hiddenTools`；
工具列表与 `rules` 用 `selectedTools - hiddenTools`；技能提示的 `fileReadTool` 增加 `"indirect"` 档
（`read`/`bash` 都被隐藏、但可通过其它工具触达时，提示不再点名具体工具）；
`ToolLoadout` 新增 `getPromptGuidelines(name)`，`codemode` 把每个工具的 prompt guidelines
跟它的声明一起展示。

**prux 现状**：
- 规则面**已满足**：`src/core/system_prompt.rs:150-160` 起的 `has_bash` / `has_grep` / … 与
  Available tools 段都基于 `compose_tools()` 过滤后的集合，隐藏工具不会出现在规则里。
- 技能面**未满足**：`format_skills_for_prompt`（`src/core/system_prompt.rs:66` 起）硬编码
  `"Use the read tool to load a skill's file …"`，且 `read` 被隐藏时整段技能段被省略
  （`:139` / `:250`），没有 pi 的 `bash` / `indirect` 降级。
- `prompt_guidelines` 目前只在系统提示里用（`system_prompt.rs:31`、`:199`）；
  `src/extensions/codemode/declarations.rs:61 render_tool_sample` 不内联各工具的 guidelines，
  也没有 pi 的 `ToolLoadout::getPromptGuidelines()` 等价物。

**建议**：`format_skills_for_prompt` 增加第三档措辞（`read` → `bash` → 不点名），
在 `compose_tools` 隐藏 `read`/`bash` 时按 pi 的规则选择档位；
`codemode` 的 `render_tool_sample` 拼接该工具的 `prompt_guidelines`。

> **状态：已修复（技能提示部分）**。新增 `SystemPromptOptions::hidden_tools` 与
> `SkillFileReader`（read → bash → 不点名），`compose_tools` 不再预过滤 `selected_tools`。
> codemode 内联 guidelines 见 2.22（未做）。断言见 `skills_hint_*` 四条。

---

### 1.7 🔧 MCP OAuth 登录无法取消、会话结束后仍在跑、缺少每请求超时

**pi 1.1.0 Fixed**（#10565）：登录每一步按 Esc 都可取消；`session_shutdown` 会中止进行中的登录；
对授权服务器的每个请求 15 秒超时；`authorizeMcp()` / `registerClient()` / token 与 discovery
函数新增 `signal` 选项。

**prux 现状**：`src/extensions/mcp.rs:1362 spawn_login()` 在后台任务里跑登录，
`login_with_browser`（`:1390`）与 `receive_callback`（`:1504`）**没有取消入口**，
回调等待固定 600s；`src/extensions/mcp/oauth.rs:526 begin_login()` / `finish_login()` 无 `signal` 参数；
HTTP 超时是 `OAUTH_HTTP_TIMEOUT = 30s`（`oauth.rs:56`），只有 `probe_oauth_challenge`
（`mcp.rs:1409`）有 15s。会话关闭不会中止登录任务。

**建议**（成本 L）：给登录流程加 `CancellationToken`（prux 已有该模式，
见 `mcp.rs:690` 的 `run_cancellable`）：
① 登录面板的取消键 → `cancel.cancel()`；② `session_shutdown` → 中止；
③ 每个对授权服务器的请求用 15s 超时（把 `OAUTH_HTTP_TIMEOUT` 拆成「discovery/refresh/signin」三个口径）；
④ 把 token 刷新与「请求 abort」分开——刷新一旦开始必须完成并落盘（同 1.3）。

> **状态：已修复**。对授权服务器的每个请求 15s（`OAUTH_HTTP_TIMEOUT`）；
> 新增 `CancelAwareOAuthHttpClient` 与 `receive_callback(.., cancel)`；登录登记到
> `core::extensions::register_background_cancel`，Esc / Ctrl+C / 会话关闭 / 退出统一取消。
> 断言见 `receive_callback_aborts_on_cancel`、`idle_escape_cancels_background_work`、
> `core::extensions::cancel::tests::*`。

---

### 1.8 ❌ MCP 会话关闭时不等连接中的服务器

pi 1.0.4 修的是 JS 侧 `McpServerConnection.close()` 的时序。prux 的 MCP 传输层委托给 `rmcp`
（`Cargo.toml` 声明 3.1.x，`Cargo.lock` 实锁 3.5.1），`StreamableHttpClientTransport` 的关闭语义
由 rmcp 负责，prux 侧没有对应实现。**结构性不适用**，升级 rmcp 时顺带复核即可。

---

### 1.9 🔧 全屏文本选择在切换会话 / transcript 重建后仍然留存

**pi 1.1.0 Fixed**（#9311 / #10567）：新增 `TuiAltScreen.resetTextSelection()`，
宿主在替换 transcript 前调用，丢掉文本选择与多击状态。

**prux 现状**：`src/modes/interactive/app.rs:649 MouseSel::sel` 保存选区。
`on_session_switched`（`handlers/events.rs:109`）、`cmd_new`（`handlers/commands.rs:425`）、
`on_navigate_done` / `on_rewind_done` 都清了 messages / 展开状态 / streaming，
但**没有一处**重置 `mouse.sel`。旧选区会命中新 transcript 里的无关文本。`reset` 类方法 grep 为 0。

**建议**：在上述四个入口（以及一切重建 transcript 的路径）统一调用
`self.mouse.sel = None; self.mouse.dragging = false;`（或抽一个 `App::reset_text_selection()`）。
补一条「切会话后没有残留选区」的测试。

> **状态：已修复**。新增 `App::reset_text_selection()`，在切会话 / `/new` / 树导航 / `/rewind`
> 四处 transcript 重建入口调用。断言见 `session_switch_resets_text_selection`。

---

### 1.10 🔧 Termux 下剪贴板粘贴无效、复制失败缺 Termux:API 提示

**pi 1.1.0 Fixed**（#10391）：`readClipboardText()` 先看 `TERMUX_VERSION`（Termux 上报的是
`android` 而不是 `linux`），并把它提到 `platform() === "linux"` 判断**之外**；
`copyToClipboard()` 失败时的 Termux:API 提示同理移出 linux 分支。

**prux 现状**：`src/utils/clipboard.rs:87-88` 有 Termux:API 的**复制**失败文案，
但**没有** `termux-clipboard-get` / `termux-clipboard-set` 命令，粘贴仍走 `wl-paste` / `arboard`，
在 Termux（`TERMUX_VERSION` 存在、通常是 android）下大概率无效。

**建议**：加 `termux-clipboard-get` / `termux-clipboard-set` 作为最高优先级命令，
判断条件用 `TERMUX_VERSION` 而非 `target_os`。

> **状态：已修复**。新增 `is_termux()`（按 `TERMUX_VERSION`，非 `target_os`）与
> `termux-clipboard-get` / `termux-clipboard-set` 读写路径。断言见
> `termux_detection_follows_the_env_var`。

---

### 1.11 ✅ 语法高亮多行字符串/注释从第二行起丢色

pi 1.0.4 修的是 JS `syntax-highlight.ts` 的 HTML 路径（formatter 逐行应用）。
prux 用 `syntect`/自带 session：`src/modes/interactive/render/syntax_hl.rs:65 HighlightSession`
跨行保状态，`render/markdown.rs:623` 每个 fenced block 建一次 session 后**逐行**喂入
（`:642-653`），多行字符串/注释不会掉色。**已满足**。

---

### 1.12 ✅ `!` / RPC bash 输出残留颜色码碎片

pi 1.1.0 修的是流式分块场景：颜色码被切在两个 chunk 之间时会留下孤立的 `m`，
新增 `splitIncompleteAnsiSuffix()` 把未完结的转义序列挂起到下一个 chunk。
prux 的 `!` 走 `handlers/tools.rs:104 run_shell_command`，用 `Command::output()`
**整体**捕获输出后才 `sanitize_output`（`render/messages.rs:1644`），不存在切块边界。**结构性不适用**。

---

### 1.13 ✅ Markdown 链接在 Herdr 里不可点击

pi 1.1.0 把 `TERM_PROGRAM=herdr` 加进 OSC 8 支持白名单。
prux 的判定是**黑名单**：`src/utils/terminal_caps.rs:152-166` 只要不是 tmux/screen 就
`hyperlinks = true`，herdr 自然为真。**已满足**（无需显式分支）。

---

### 1.14 ✅ 独立二进制从启动目录加载 `.env`

prux **从不加载** `.env`（`src/main.rs` 无 dotenv/dotenvy，`Cargo.toml` 无该依赖）。
**无此问题**。

---

### 1.15 ❌ `pi update` 的 managed 安装保留所有旧版本

prux 的 `src/extensions/update_check.rs` 只做 GitHub release 检查 + 打开下载页
（`command_update`），没有 managed 安装与更新器。pi 1.1.0 新增的
`pruneManagedReleases()`（只保留当前版本与「正在运行的那个版本」）**不适用**。

---

### 1.16 ✅ codemode 在 pnpm 全局更新后整个会话失败 + 重启提示

pi 1.0.3 修的是「运行中的安装被 pnpm 更新移除」：把 QuickJS wasm 路径与 codemode worker
缓存为 data: URL，并新增 `detectInstallChange()` 在出错时提示重启。
prux 的 codemode 用**内嵌 rquickjs**（`src/extensions/codemode/sandbox.rs`），
没有外部 JS 安装可被移除。**结构性不适用**。

---

### 1.17 ⚠️ 终端消失后报 `read EIO` / `setRawMode EIO` 崩溃

**pi 1.1.0 Fixed**：`DEAD_TERMINAL_ERROR_CODES` 增加 `ENOTTY`，并统一识别「终端已消失」
这一类错误，避免在关窗 / 在已关闭终端里恢复挂起的 pi 时崩栈并提示跑 `/bug`。

**prux 现状**：主事件循环 `src/modes/interactive.rs:257` 的
`let Some(Ok(ev)) = ev else { continue }` 会吞掉读错误；但 `render_tick(...)?`（`:236`）
与启动期的 `enable_raw_mode()?`（`:417`，报 "failed to initialize terminal"）仍会把错误冒出。
没有统一的 EIO/ENOTTY 识别。

**建议**：抽一个 `is_dead_terminal_error()`（EIO / EPIPE / ENOTCONN / ENOTTY），
渲染与输入路径命中时走「安静退出」而不是抛错。

> **状态：已修复**。新增 `is_dead_terminal_error()` / `is_dead_terminal_io()`
>（EIO / EPIPE / ENOTCONN / ENOTTY + BrokenPipe/NotConnected/UnexpectedEof），
> 启动、渲染与读流三处命中即安静退出；`Error::Io { context, source }` 保留错误链。
> 断言见 `dead_terminal_errors_are_recognized`。

---

### 1.18 ⚠️ 计算输出上限时按 3.5 字符/token 估算输入

**pi 1.1.0 Changed**（ai #10497）：`CHARS_PER_TOKEN` 4 → 3.5，减少上下文长度类请求失败。

**prux 现状**：全库没有 3.5 常量，也**没有** pi 那种「按估算输入钳制输出上限」的机制：
`src/core/provider.rs:1418` 起的 `max_tokens` 原样透传到请求体，没有 `clampMaxTokensToContext()` 等价物。
prux 的上下文估算走 `tiktoken` 真实 BPE（`src/core/compaction/utils.rs:19`），与 pi 的字符启发式不是同一条路。
`codemode/description.rs:14 CHARS_PER_TOKEN = 4` 是工具描述预算，与本题无关。

**建议**：这项**不是**「把 4 改成 3.5」那么简单。先在 prux 里确认自己是否需要用估算值钳制
`max_tokens`；若要补，用 tiktoken 的调用计数而不是字符启发式，并单独写清与 pi 的差异。

> **状态：已修复**。新增 `provider::clamp_max_tokens_to_context`（`src/core/provider.rs`），
> 在 [`stream_chat`] 分发协议前把输出上限钳到「`context_window` − 已用上下文 − 4096 安全余量」；
> 同时把 token 估算上移到 `src/core/provider/estimate.rs`（provider 层成为唯一真源，
> `compaction` 改为转发，避免两份算法分叉）。与 pi 的差异：pi 用 `chars / 3.5` 启发式，
> prux 用 tiktoken BPE 真实计数，且把 system 提示与工具声明也计入（pi 的 system 在 transcript 里）。
> 断言见 `max_tokens_is_clamped_to_context_room`、`clamp_keeps_explicit_no_limit`、
> `clamp_skips_context_room_when_window_unknown`、`declared_tools_and_system_prompt_count_toward_context`。

---

### 1.19 ✅ Mistral 响应 `finish_reason: "error"` 不重试

prux 未实现 Mistral provider（`SUPPORTED_PROVIDERS` 无 mistral，`login_registry.rs:9`
明确「排除协议未实现 provider」）。pi 的修复在 `mistral-conversations.ts`，**不适用**。

---

### 1.20–1.23 ❌ 无对应 provider 的修复

- **1.20 Bedrock**：`The pending stream has been canceled` 不重试、Converse 不向 OpenAI 模型发 reasoning effort、
  以及 OpenAI on Bedrock 忽略 thinking level —— prux 无 Bedrock provider。注：prux 的
  `RETRYABLE` 里有 `"http2 request did not get a response"`（`agent_session.rs:2292`），
  若将来接 Bedrock，需要补 `"pending stream has been canceled"`。
- **1.21 `openai-codex` 的 `originator` / `User-Agent` 不可被 models.json 覆盖** —— prux 已删除
  `openai-codex` provider（`model_resolver.rs:1996` 附近），无硬编码头。不适用。
- **1.22 Radius 被组织管理员禁用的模型仍被列出** —— prux 不支持 Radius。注：`model_resolver.rs:106-130`
  的目录动态合并是 **upsert 合并**；将来接 Radius 时必须改成「网关目录整体替换基线目录」。
- **1.23 lazy API setup 失败用失败时刻当 `timestamp`**、**faux provider 缓存估算** —— 无对应实现。

---

### 1.24 🔧 内置模型成本缺少 prompt-length 定价档

**pi 1.1.0 Fixed**：补齐 OpenCode / OpenCode Go / OpenRouter / Vercel AI Gateway / Google / MiniMax
等 models.dev provider 的「按 prompt 长度分档」定价，长 prompt 的成本不再少算
（Claude Haiku 5.5 / Gemini 3.1 Pro / GPT-5.4 等）。

**prux 现状**：**引擎已支持**——`src/core/provider/usage.rs:34 compute_cost()` 会读
`cost.tiers[].inputTokensAbove` 选最高命中档，并有测试。**差距在数据**：
`assets/models/models.all.json` 里只有 `github-copilot`（12）/ `openai`（11）/ `xai`（4）带 `tiers`，
OpenCode / OpenRouter / Vercel / Google / MiniMax **全部为 0**。
`scripts/sync-models.py` 原样拷贝该字段，所以这是 1.0.2 数据同步时留下的旧数据。

**建议**：重跑 `make sync-models`（见 2.16 / 2.17 一起做），然后 `scripts/sync-models.py --check` 验零差异。

> **状态：已修复（数据已同步）**。`assets/models/**` 已按 pi-ai 1.1.0 重跑同步：
> 带 `cost.tiers` 的模型从 27 条涨到 189 条（openrouter 79 / vercel-ai-gateway 49 / opencode 16 …），
> `claude-haiku-5-5` 一并带入（2.16 数据面）。`scripts/sync-models.py --check` 零差异。
> 注：本次同步也带入了 `openai/gpt-6-luna`（`api: openai-decisions`）——prux 未实现该协议，
> 它会被列为可用分类器、调用时返回 `provider openai does not support classification`（见 2.17）。

---

### 1.25 ❌ OpenAI provider 用不含 classifier 的缓存目录做类型检查

prux 的目录结构与类型检查与 pi 不同（无 `KnownClassifierApi` 的 TS 收窄），**不适用**。

---

### 1.26–1.28 ❌ 其余无对应实现的修复

- **1.26** Node 24.19+/26.x 下 `node --watch` 让图片被当成 "could not be resized" —— Node 专有。
- **1.27** `StreamableHttpTransport.close()` 的 DELETE 复用最后一次请求的 token —— 委托 rmcp。
- **1.28** `!!` 命令头部输出到达后掉 dim 色 —— prux 的 `!!` 走 `push_msg(Info)` 整条 dim
  （`handlers.rs:651-656`），不是「头部先创建、输出后来追加」的结构，不适用。

---

### 1.29 ⚠️ MCP 动态客户端注册在 OIDC 服务器上报 `invalid_redirect_uri`

**pi 1.0.4 Fixed**（#10493）：`registerClient()` 现在发送 `application_type`
（MCP SEP-837），由 `redirect_uris` 派生：loopback host 与自定义 scheme → `native`，否则 `web`。

**prux 现状**：prux 的注册请求由 rmcp 发出。rmcp 3.5.1 默认
`application_type = "native"` 并写进注册请求（`rmcp transport/auth.rs`），
所以 loopback 场景（prux 的主要场景）**已被修好**；但 prux 的 `OAuthConfig`
（`src/extensions/mcp/config.rs:294` 起）没有该字段，也没有「按 redirect_uris 派生 native/web」的逻辑。

**建议**：若只做 loopback 登录，现状可用；若要支持自定义 scheme / 非 loopback redirect，
需要在 `OAuthConfig` 增加 `application_type` 并在注册时透传。升级 rmcp 时复核默认值。

> **状态：部分满足（依赖 rmcp 默认值）**

---

### 1.30 ❌ `/mcp` 面板不再等所有服务器连接完成

prux 的 `/mcp` 是斜杠命令 `command_mcp`（`mcp.rs:990`，声明为 `busy_safe`），
status/list 只读缓存、连接是惰性的，test/login 走 `tokio::spawn` 后台通知。
没有 pi 那种「打开前等全部连接」的面板，**不适用**。

---

## 二、本区间新增 / 变更功能（候选移植）

### 2.1 🆕 `--tools` / `--exclude-tools` 支持 `*` 通配

**pi 1.0.4 新增**：两个参数都接受 `*`（匹配任意字符），例如
`--tools read,codemode,'mcp__radius__*'`。实现是 `createToolNameMatcher(entries)`
（`packages/coding-agent/src/core/mcp-servers.ts`）：精确名进 `Set`，含 `*` 的转成正则。

**prux 现状**：`src/cli/args.rs:106-110` 只有 `value_delimiter = ','`；
`src/core/agent_session.rs:127-134` 的 `ToolSelection::is_allowed()` 用
`list.iter().any(|n| n == name)` **纯精确匹配**，`compose_tools()`（约 170 行起）同样精确。
另外 prux 的 `--exclude-tools` **没有** pi 的 `-xt` 短名。

**建议**：新增 `ToolNameMatcher`（精确集合 + 通配正则），替换 `ToolSelection` 里的两处比较；
把 `ToolSelection::Allowlist(Vec<String>)` 改成持有 matcher，或让 `is_allowed` 内部编译一次。
同时补 `-xt` 短名。注意 `--tools` 现在没有校验，`+name` 混用要报错（见 2.3）。

> **状态：已实现**。新增 `core/tool_names.rs`（`wildcard_matches` / `matches_any`，手写通配现算），`--tools` / `--exclude-tools` 的内置选择改为按 matcher 过滤注册表，并补 `-x` 短名。

---

### 2.2 🆕 `--tools` 保留 MCP 工具 + 新增 `--no-mcp`

**pi 1.0.4 新增**：`--tools` 此前会把 MCP 工具一起挡掉（`pi --tools codemode` 之后没有任何 MCP 服务器）；
现在**保留** MCP 工具，除非某个条目以 `mcp__` 开头；空 allowlist（`--no-tools`）仍然挡掉。
新增 `--no-mcp` 完全关闭内置 MCP 支持。
语义细节：被 allowlist 保留但未被点名的 MCP 工具**只注册给 codemode / tool_search**，
不对模型声明（pi 的 `_isActivatable()`），且 `direct` exposure 的 MCP 工具只有在
`tool_search` 已注册时才可激活。

**prux 现状**：`ToolSelection::Allowlist` 会挡掉包括 MCP 在内的全部工具；无 MCP 例外，
无 `--no-mcp`（`grep no_mcp` 为 0）。prux 的 MCP 服务器由内置 `mcp` 扩展提供，
关闭它需要走 `--no-extensions` 之外的专门开关。

**建议**：① `is_allowed` 增加「allowlist 未命中且 name 是 MCP 工具 → 保留注册」的分支
（`mcp__` 前缀 + `list_mcp_resources` / `list_mcp_resource_templates` / `read_mcp_resource` 三个资源工具，
对齐 pi 的 `isMcpToolName()`）；② 新增 `--no-mcp` → 在资源加载层禁用内置 `mcp` 扩展
（prux 的扩展注册是声明式的，见 `src/extensions.rs`，过滤一条清单即可）。
`--exclude-tools` 对 MCP 工具依旧生效。

> **状态：已实现（与 pi 有结构差异）**。prux 的 MCP 是单个聚合网关工具，未点名时**保留注册但不对模型声明**（脚本与嵌套调用仍可达），点名或用 `--exclude-tools mcp` 才挡掉；新增 `--no-mcp`。

---

### 2.3 🆕 `--tools` 支持 `+name` / `-name` 增量条目

**pi 1.1.0 新增**：`pi -t +codemode,-write` 在默认选择上增减，而不是整体替换。
纯 `+`/`-` 列表与普通名/通配**不能混用**（`getToolListError()`），且 `+`/`-` 只接受**精确名**，
不接受 `*`。`createAgentSession` 的 `tools` 选项、`/reload` 重读 `defaultTools` 都套用同一套修饰符。

**prux 现状**：settings 的 `defaultTools` **已经**支持 `+name` / `-name`
（`src/core/settings_manager.rs:512-560`：`merge_default_tools` / `resolve_default_tools`），
但 `--tools` CLI 解析（`args.rs`）与 `ToolSelection` 完全不知道修饰符，会把它当普通工具名。
`/reload` 重读 `defaultTools` 的逻辑在 `agent_session.rs:3662` 附近已有。

**建议**：把 `resolve_default_tools` 的修饰符逻辑抽成公共函数，供
① `args.rs` 校验（纯修饰符 vs 混用 vs 含 `*` 的报错）与
② `main.rs:255` 构造 `ToolSelection` 时复用（修饰符 → `Default(apply(base, modifiers))`）。
`/reload` 时把 CLI 给出的修饰符再套到重读后的 `defaultTools` 上（`AgentSession` 需要额外记住它们）。

> **状态：已实现**。`--tools +name/-name` 在 main 求值，`/reload` 按同一组修饰符重算（对齐 pi 的 `defaultToolModifiers`）。

---

### 2.4 🆕 codemode `image()` 把每张图写进临时文件并在结果里给出路径

**pi 1.0.3 Changed + 1.0.4 Changed**：`image()` 现在把每张图片落到
`<tmp>/pi-codemode-<hex>.<ext>`，结果里在图片前插一条 `[Image saved to <path> (<mime>, <size>)]` 文本项；
写失败时把原因写进标签而不丢弃脚本结果。落盘统一走新的 `utils/output-files.ts`
（权限 `0o600`、`flag: "wx"`、不会跟随他人预置的符号链接）。

**prux 现状**：`src/extensions/codemode.rs:960-998 run_script()` 把 `ScriptOutput::Image`
直接转成 `ToolResultAttachment`，**没有落盘**，也没有路径文本项；
`src/utils/` 下没有统一的「输出文件」模块。

**建议**：新增 `src/utils/output_files.rs`（`write_output_file(prefix, ext, bytes)`，
`mode 0o600` + `OpenOptions::create_new`，避免 TOCTOU 跟随符号链接），
在 `run_script` 的图片分支前插一条文本项；同一张图（按 base64 去重）只写一次。
bash 截断输出的临时文件（`tools/bash.rs`）与 MCP 二进制资源也可以顺带收敛到同一模块。

> **状态：已实现**。新增 `utils/output_files.rs`（`0o600`、独占创建、不随句柄删除），`image()` 落盘并在文本里给出可读路径。

---

### 2.5 🆕 codemode `tools.read()` 读到图片时返回 image block

**pi 1.0.4 新增**：`read` 工具声明 `outputSchema`（`string | { type: "image", data, mimeType, note }`）
并在结果里带 `structuredContent`；codemode 的 `tools.read()` 因而不只是拿到文字，
而是拿到 `image()` 能直接展示的图片块。

**prux 现状**：`src/core/tools/read.rs:242-300 read_image_result()` 只返回 text + attachment，
**没有** `output_schema` / `structured_content`；codemode 的宿主侧
`Host::script_value`（`codemode.rs:309-331`）只认 `structured_content` 或纯文本，
所以脚本拿到的 `tools.read()` 结果里没有图片。

**建议**：给 `read` 增加 output schema 与 `structured_content`
（文本文件 → 字符串；图片 → `{ type, data, mimeType, note }`），
让 `script_value` 原样透出。注意 `note` 是图片旁的提示文本（如「模型不支持视觉」），
要与 2.4 的落盘路径配合。

> **状态：已实现**。`read` 声明 `output_schema` 并在图片分支返回 `structured_content`，`Host::script_value` 原样透出。

---

### 2.6 🆕 codemode 冻结内建 + 畸形 payload 以 sandbox 错误失败

**pi 1.0.4 Fixed/Changed**（#10444）：脚本运行前 `lockdown()` 冻结所有可达内建
（含 iterator / generator / %TypedArray% 原型），常用可覆写属性转成 accessor 以避开
「override mistake」；`Array.prototype.toJSON = …` 之类的补丁不再生效。
宿主侧对 worker 传来的 payload 做严格校验，畸形 payload（含 `toJSON` 被篡改导致的）
以 `sandbox` 错误失败，而不是让宿主进程崩栈、`execute()` 永不 settle。

**prux 现状**：`src/extensions/codemode/glue.js` 只 `Object.freeze` 了
`tools`（`:128`）、`allTools`（`:129`）、命名空间成员（`:201`）、`console`（`:412`），
**没有 lockdown 循环**，`Array.prototype.toJSON` 之类的补丁仍然生效。
好消息是 prux 用**进程内 rquickjs**（`sandbox.rs`），畸形 payload 不会崩宿主：
`finish()`（`sandbox.rs:518-560`）在 serde 失败时降级，`__settle` 失败走 `ErrorKind::Sandbox`
（`sandbox.rs:347-356`）。

**建议**：把 pi 的 `lockdown()` 逻辑移植进 `glue.js`（保留 `OVERRIDABLE` accessor 处理，
否则 `class MyError extends Error { name = "x" }` 会抛）；宿主侧的 payload 校验做一层
显式 `Result`（当前依赖 rquickjs 的异常，语义上等价但错误文案不够明确）。

> **状态：已实现**。pi 的 `lockdown()` 已移植进 `glue.js`（含 `OVERRIDABLE` accessor，override mistake 不复发）。宿主侧 payload 校验仍依赖 rquickjs 异常（进程内不崩，语义等价但文案不如 pi 明确）。

---

### 2.7 🆕 codemode `console` 标记 + 输出项分隔格式

**pi 1.1.0 新增**（codemode 包）：`console.*` 产生的 text item 带 `console: true`，
宿主可区分它与 `text()`。**pi 1.1.0 Fixed**（coding-agent）：多个 text item 现在每个以
`==> text N/M <==` 行开头，`console` 行集中到一个 `<console_output>` 块（每行一次调用），
避免 provider 把相邻文本块直接拼起来、模型分不清边界。

**prux 现状**：`glue.js:408` 的 `console[level]` 仍调用 `output("text", …)`；
`ScriptOutput`（`sandbox.rs:93-102`）**没有** Console 变体；
`run_script` 把文本直接 push 进 `texts`，`truncate_output`（`codemode.rs:1109`）只 `join("\n")`，
没有 `==> text N/M <==` 与 `<console_output>` 包装。

**建议**：`ScriptOutput` 增加 `Text { text, console: bool }`（或独立变体），
`run_script` 在拼装最终 content 时套 pi 的 `formatOutput()` / `joinAdjacentText()`：
多 text 项加分隔头、console 归并成一块。注意分隔头的计数只算非 console 的 text 项。

> **状态：已实现**。`ScriptOutput::Console` + `format_output` / `join_adjacent_text` / `save_images`；描述与文档同步改口径。

---

### 2.8 🆕 codemode `models.classify()` 支持 images

**pi 1.1.0 新增**（ai + coding-agent）：`ClassifierContext` 增加可选 `images`；
`assertClassifierInputSupported()` 对目录里 `input` 不含 `"image"` 的模型直接返回错误结果；
codemode 的参数校验新增 `context.images` 的形状检查。

**prux 现状**：`ClassifierContext`（`src/core/provider/classifier.rs:72-77`）只有 `state` + `questions`；
`codemode.rs:457-471` 的 `classify` 直接 `from_value`。没有 input 能力校验。

**建议**：`ClassifierContext` 加 `images: Option<Vec<ImageBlock>>`，
`classify()` 里在模型 `input` 不含 `image` 且 images 非空时返回错误结果
（而不是发出去被 provider 拒），codemode 侧的 `checkClassifierContext` 同步加形状校验。

> **状态：已实现**。`ClassifierContext.images` + 入口的能力检查 + `classifier_context_arg` 形状校验（对齐 pi 的 `checkClassifierContext`）。

---

### 2.9 ✅ codemode 描述里的 `searchTools()` / `describeTool()` / `describeNamespace()` 未标 async

**pi 1.1.0 Fixed**：模型会把未 `await` 的 promise 序列化成 `{}`，现在描述里带上 `await`。
prux 的 `src/extensions/codemode/description.rs:70`（`INTRO` 常量）**已经**写成
`await searchTools(…)` / `await describeTool(name)` / `await describeNamespace(name)`。
**已满足**（上一轮迁移已处理）。

---

### 2.10 🆕 工具渲染上下文新增 `durationMs`

**pi 1.1.0 新增**（agent + coding-agent）：`AgentToolCallOutcome`、`tool_execution_end`
扩展事件、`ToolRenderContext` 都新增 `durationMs`（单调时钟、排除 hooks；
未运行的调用没有该字段）。`ToolRenderContext.outputPad` 见 2.11。

**prux 现状**：`duration_ms` 已存在于 toolResult 消息与 `ToolRenderCtx`
（`src/core/extensions/renderers.rs:40`），嵌套调用也带（`agent_session.rs:3931/3952`）。
**缺**：`tool_execution_end` 事件不带 `durationMs`
（`agent_session.rs:3396-3410`，测试 7130 行还显式断言「pi 无 durationMs 字段」），
`AssistantMessage` 的 `duration_ms` 恒为 `None`
（`anthropic.rs:1361`、`completions.rs:1451`、`responses.rs:1574`、`google.rs:965` 全传 `None`）。

**建议**：① `emit_tool_execution_end` 带上 `durationMs`；② 各 provider 在流开始/结束时
用单调时钟填 `AssistantMessage.duration_ms`；③ 把 7130 行那条「无 durationMs」断言改成
「有/无的正确语义」。顺带把 1.5 的测量点收窄到 `execute()`。

> **状态：已实现**。`FinalizedToolCall.duration_ms` 进 `tool_execution_end`；流式 assistant 消息的 `durationMs` 由 `stream_chat` 统一盖。

---

### 2.11 🆕 `outputPad` 扩展到 `!` 命令输出 / 工具输出 / summary 块

**pi 1.1.0 新增/Changed**：`ToolRenderContext` 新增 `outputPad`
（`renderShell: "self"` 的渲染器自行应用）；`outputPad` 语义从「chat message output」
扩为「transcript content」，并应用到 `!` 命令输出、工具输出与 summary 块
（`Box` / `Text` 新增 `setPaddingX()`，`BashExecutionComponent`、`CustomEntryComponent`、
`CompactionSummaryMessageComponent`、`BranchSummaryMessageComponent`、
`SkillInvocationMessageComponent` 都接线）。

**prux 现状**：渲染层把 1 列缩进**硬编码**（`render/messages.rs:5` 注释即 `outputPad=1`；
`bg_line(..., 1)` 用于工具输出 `:1644`、summary `:2126`、子调用 `:1802`）。
**没有** `outputPad` 设置项，也没有 `settings.json` 读写的对应键（`settings_manager.rs` grep 为 0），
`ToolRenderCtx` 也无该字段。

**建议**：若决定移植，先在 `settings_manager` 加 `outputPad`（`0 | 1`，默认 1）与 `/settings` 项，
再把 `render/messages.rs` 里硬编码的 1 换成该值，并加进 `ToolRenderCtx`。
若暂不移植，需在文档里注明 prux 的缩进不可配置，避免与 pi 面板项对不齐。

> **状态：未做（有意跳过）**。prux 无 TUI 组件层，落地要给出参数量已超长、多处 `too_many_arguments` 的消息渲染函数链各加一个参数；按本条「若暂不移植需注明」的要求，第九节记录结论：**prux 的 transcript 缩进硬编码为 1 列、不可配置**。

---

### 2.12 🆕 `agent_settled` 事件新增 `aborted`

**pi 1.1.0 新增**：会话事件、扩展事件、JSON 事件三处的 `agent_settled` 都带 `aborted`，
集成方据此区分「被取消的运行」与「正常完成」。pi 的 `ProgramStatusReporter` 靠它把
取消后的状态回落成 `idle`。

**prux 现状**：`src/core/agent_session.rs:2345` 只发 `json!({ "type": "agent_settled" })`，
**无载荷**；UI 侧 `handlers/events.rs:741 enrich_agent_settled` 只补 `hasQueuedMessages`。
prux 的取消状态由别的字段表达（`agent_session.rs:2330` 附近注释区分了
`agent_settle` 与「整轮结束」）。

**建议**：`emit_agent_settled` 带上 `aborted`（取本轮是否被中断请求），
UI 侧可据此收尾；这是 2.13 的前置。

> **状态：已实现**。`emit_agent_settled` 带上本轮的取消信号（`aborted`）。

---

### 2.13 🆕 Program Status（OSC 7501）

**pi 1.1.0 新增**（#10607）：tui 包新增 `Terminal.setProgramStatus()` /
`formatProgramStatus()`（`packages/tui/src/program-status.ts`），启动时探测终端支持，
只有终端应答过才发；`PI_PROGRAM_STATUS=1|0` 可覆盖。coding-agent 新增
`ProgramStatusReporter`（`modes/interactive/program-status-reporter.ts`），
上报 `working` / `blocked`（扩展对话框或登录）/ `done` / `error` / `idle`，
消息只含会话名、对话框标题与错误首行，**绝不含 prompt 或模型输出**。
tmux / screen 不转发该序列；文档见 `docs/terminal-setup.md#program-status`。

**prux 现状**：**完全缺失**（`grep 7501` 为 0；无 `ProgramStatus` / `PI_PROGRAM_STATUS`）。
prux 基于 ratatui，没有 pi 的 `Terminal` trait，所以 pi 的
「`Terminal` 实现必须提供 `setProgramStatus`」这条破坏性变更**不适用**；
但 OSC 7501 的上报、探测与状态建模都要从零实现。

**建议**（成本 M–L，纯新增、无回归风险）：新增
`src/modes/interactive/program_status.rs`：状态枚举 + 去重上报 + `PI_PROGRAM_STATUS` 覆盖；
支持探测走「写查询序列 + 读应答」，必须**早于 crossterm 事件流建立**（与
`terminal_image` 的探测时机同类问题，见第十六节）。上报点绑定到
`agent_start` / `message_end` / `compaction_*` / `agent_settled`（2.12）与扩展对话框开合。
先做 `working` / `done` / `error` / `idle`，`blocked` 依赖对话框接线可后置。

> **状态：未做**

---

### 2.14 🆕 MCP OAuth 登录可取消 + `signal` 选项 + 每请求 15s 超时

见 1.7 的补丁（同一批改动，pi 同时把它算作新 API）。

---

### 2.15 ❌ `pi mcp login --timeout` 限制整段登录

pi 1.1.0 改的是**已有**的 `--timeout` 语义（此前只限制等浏览器）。prux 的
`mcp login` 用法是 `prux mcp login <name> [--no-browser]`（`src/cli/mcp_command.rs:54`），
**没有** `--timeout`；`--timeout-ms` 是 `mcp add` 的单次 RPC 超时，两回事。
若要移植 2.14，可顺带加 `--timeout`（默认值对齐 pi）。**当前不适用**。

---

### 2.16 🆕 Claude Haiku 5.5

**pi 1.1.0 新增**：`anthropic/claude-haiku-5-5`，带 prompt-length 定价档、
adaptive thinking 支持到 `xhigh` / `max`、逐消息 effort、对话中途的系统消息与工具变更；
Bedrock 上走 adaptive thinking + native `xhigh` + prompt caching。

**prux 现状**：`assets/models/models.all.json` 里**没有** `claude-haiku-5-5`。
目录数据由 `make sync-models` 从 pi-ai 生成，`sync-models.py` 原样拷贝字段，所以
重跑同步即带入（含 `tiers` 定价档，见 1.24）。
「对话中途系统消息 / 工具变更」是 anthropic 协议侧能力，需要确认 prux 的
`src/core/provider/anthropic.rs` 是否已按 pi-ai 1.1.0 的形状发送（属目录之外的工作）。

**建议**：重跑同步；然后核对 anthropic 的 adaptive thinking 与 mid-conversation 系统消息路径。

> **状态：数据面已同步**。`claude-haiku-5-5` 已进目录（含 `tiers` 定价档，见 1.24）。
> 「对话中途系统消息 / 工具变更」仍待核对（`anthropic.rs` 是否按 pi-ai 1.1.0 的形状发送）。

---

### 2.17 🆕 GPT-6 Luna 分类器（`openai-decisions` API）

**pi 1.1.0 新增**：新增 `openai-decisions` classifier API（OpenAI Decisions API），
`gpt-6-luna` 作为 `openai` provider 的 classifier 模型；需要 API key，
用 Sign in with ChatGPT 登录时不列出。`packages/ai/src/api/openai-decisions.ts`。

**prux 现状**：prux **区分** chat / image / classifier
（`ModelType::{Chat, Image, Classifier}`，`src/core/provider.rs:190`；
`list_models_of_type` / `find_model_of_type` 在 `model_resolver.rs`），
但**唯一内置的分类器协议是 `typesafe-system-one`**（`src/core/provider/classifier.rs:3`、`:146`），
没有 `openai-decisions`。目录里 `openai` 也没有 `gpt-6-luna` 的 classifier 条目。

**建议**：新增 `openai-decisions` 协议实现（非流式 POST，形状参照 pi 的 `openai-decisions.ts`），
dispatch 分支加到 `provider.rs:876-916` 附近；目录同步后 `gpt-6-luna` 会自动出现；
「ChatGPT 登录时不列出」的判定加到 `model_resolver` / `/models` 的可用性过滤里。

> **状态：已实现**。新增 `core/provider/decisions.rs`（`classify_openai_decisions`），
> dispatch 分支在 `provider::classify`；「ChatGPT 登录不列出」落在
> `model_resolver::classifier_hidden_by_chatgpt_sign_in`（列表与查找同一口径）。
> 断言见 `decisions::tests::*`、`openai_classifier_listing_follows_chatgpt_sign_in`。

---

### 2.18 🆕 Azure：provider 重命名 + Foundry Chat Completions

**pi 1.0.3 破坏性 + 新增**：provider 从 `azure-openai-responses` 改名为 `azure`
（因为它现在同时服务 Responses 与 Chat Completions）；新增 Foundry Chat Completions 部署，
首个内置模型 `azure/deepseek-v4-pro`；其它 Foundry 模型可用 `api: "openai-completions"` 添加；
`AZURE_OPENAI_DEPLOYMENT_NAME_MAP` / `azureDeploymentName` 对两种 API 都生效。
`AZURE_OPENAI_*` 环境变量不变。coding-agent 侧同步：`defaultModelPerProvider.azure = "gpt-5.4"`。
**注意**：`pi` 的迁移说明明确「旧 provider 名登录的会话恢复时回落到别的模型、prompt cache 不复用」。

**prux 现状**：prux **完全没有** azure provider —— `src/core/login_registry.rs:9`
的注释还写着「排除协议未实现 provider（azure-openai-responses / mistral）」；
`SUPPORTED_PROVIDERS`（`model_resolver.rs:31`）无 azure；`auth.rs:324 api_key_env_var` 无
`AZURE_OPENAI_*`；`assets/models/providers/` 无 `azure.json`。

**建议**：这项是**新接入**而非改名：① `SUPPORTED_PROVIDERS` 加 `azure`；
② `auth.rs` 补 `AZURE_OPENAI_API_KEY` 等环境变量映射；
③ chat 协议白名单（`provider.rs:1403/1451`）加 azure 的 Responses 与 `openai-completions`
两条（`openai-completions` 已有，主要是 base_url / deployment 名映射）；
④ `scripts/sync-models.py` 的 provider 解析会自动跟上（它读 `SUPPORTED_PROVIDERS`）；
⑤ `sync.md` 与 `/login` 注册表同步。
若暂不接 azure，本节整条可推迟。

> **状态：已实现（新接入）**。`SUPPORTED_PROVIDERS` 加 `azure` 并重跑同步（45 个条目）；
> 新增 `core/provider/azure.rs`（base URL / API 版本 / 部署名解析），dispatch 前替换 `base_url`，
> 两个协议写请求体 `model` 时改用 [`azure::request_model_name`](src/core/provider/azure.rs)；
> `auth.rs`、`login_registry.rs` 同步。断言见 `azure::tests::*`、
> `azure_resolves_endpoint_and_deployment_for_both_apis`。
>
> **与 pi 的差异（有意保留）**：prux 没有 pi 的 `azureBaseUrl` / `azureResourceName` /
> `azureDeploymentName` 三个 SDK 选项，对应的覆盖入口是环境变量与 `models.json` 的 `baseUrl`
> （解析失败文案据此改写）。

---

### 2.19 ❌ llama.cpp 原生决策模型（Julia-1 / Laya / Kev / lev / OpenJev）

**pi 1.1.0 新增**：llama.cpp 0.6.0+ 的这几个模型只作为 classifier 通过 `/v1/systemone` 列出，
不再作为 chat 模型。prux **没有** llama.cpp provider / `llama` 扩展 / `llama-cpp-classify` 协议，
而 llama.cpp 接入本身早于 1.0.2，按收录规则**不展开**。

---

### 2.20 ❌ `LoginOptions.agentName`

pi-ai 的 SDK 选项，替换「Sign in with ChatGPT」里的 pi 名称提示与 Codex 浏览器登录的 originator。
prux 是 CLI，`openai_chatgpt.rs` 直接用固定名称，无嵌入方需要覆盖。**不适用**。

---

### 2.21 ⚠️（并入 2.4）输出文件权限 `0o600`

pi 1.0.3 Changed：「输出文件（截断工具输出的全文、MCP 二进制资源、codemode 图片）现在只有用户可读」，
统一走 `utils/output-files.ts` 的 `mode: 0o600` + `flag: "wx"`。
prux 对应实现见 2.4 的建议（新增 `src/utils/output_files.rs`）；
prux 现有 bash 临时文件（`tools/bash.rs`）与 MCP 资源落盘需要一并核对权限。

> **状态：已实现**。随 2.4 一起收敛到 `utils/output_files.rs`；bash 的完整输出与 codemode 图片都走同一模块，且文件不再随句柄 drop 被删。

---

### 2.22 🆕 codemode 声明内联各工具的 prompt guidelines

**pi 1.0.4 新增**：`ToolLoadout.getPromptGuidelines(name)`；`codemode` 的
`toCodemodeDeclaration(tool, guidelines)` 把工具的 prompt guidelines 拼到描述后面，
`ALL_TOOLS` 与 `describeTool()` 因此也能看到 guidelines（系统提示只对「已声明」的工具展示它们）。

**prux 现状**：`ToolRenderCtx` 之外的 `ToolLoadout` 等价物（`src/extensions/tools.rs:297` 附近）
**没有** `getPromptGuidelines()`（grep 为 0）；
`src/extensions/codemode/declarations.rs:61 render_tool_sample` 只用工具自身的 description。

**建议**：把 `prompt_guidelines`（`system_prompt.rs:31` 已有该数据）暴露给 codemode 的
`render_tool_sample`，按 pi 的格式拼成 `description + "\n\n- guideline…"`。

> **状态：已实现**。`render_tool_sample` 接受 guidelines 并按 pi 格式拼在描述之后，两个调用点同步。

---

## 三、行为/接口变更（对齐时容易踩的坑）

| # | 变更 | prux 影响 |
|---|---|---|
| 3.1 | `--tools` 语义从「整体替换」扩展为「通配 + MCP 保留 + 可选 `+/-` 修饰符」 | 见 2.1–2.3；`--tools` 的 help 文案要一并改（`src/cli/args.rs` 的 doc comment） |
| 3.2 | `outputPad` 从「chat message output」扩为「transcript content」 | 见 2.11 |
| 3.3 | `Home`/`End` 总是移动编辑器光标；全屏 transcript 顶/底改到 `Ctrl+Home`/`Ctrl+End`，且不再移动光标 | **已改**：`cursorLineStart/End` 去掉 `ctrl+home`/`ctrl+end`；transcript 顶/底从 `shift+home`/`shift+end` 改为 `ctrl+home`/`ctrl+end`；文档同步。**破坏性**：旧键位不再生效 |
| 3.4 | Azure provider 改名 + 会话兼容性 | 见 2.18；prux 从未移植，无迁移负担 |
| 3.5 | `pi mcp login --timeout` 限制整段登录 | 见 2.15，prux 无该参数 |
| 3.6 | `codemode` 的 `console` 输出与多 text 项分隔格式 | 见 2.7；这是**模型可见**的变化，会影响已有 prompt/评测 |
| 3.7 | codemode 描述里 `searchTools` 等标 `await` | 已满足（2.9） |
| 3.8 | 脚本探测工具需用 `"name" in tools`（`typeof tools.x` 不再可靠） | 已在上一轮（1.0.0 迁移）落地，prux 的 `glue.js` 已按 Proxy + TypeError 语义实现，无需再动 |
| 3.9 | 输出文件权限收紧到 `0o600` | 见 2.21 |

---

## 四、模型目录 / 数据同步（与代码改动解耦）

1. `Cargo.toml` 的 `version` 是唯一真源；升到 1.1.0 时**必须**同时新增
   `assets/changelog/v.1.1.0.md`（缺失会 `include_str!` 编译失败）。
2. `assets/models/**` 由 `make sync-models` 生成，范围由 `SUPPORTED_PROVIDERS` 决定。
   本次同步会带入：`claude-haiku-5-5`（2.16）、`gpt-6-luna` 分类器条目（2.17，目录面）、
   OpenCode / OpenRouter / Vercel / Google / MiniMax 的 prompt-length 定价档（1.24）。
3. 若决定接 azure（2.18），**先把 `azure` 写进 `SUPPORTED_PROVIDERS`** 再同步，
   否则 `azure.json` 不会被生成。
4. 同步后跑 `scripts/sync-models.py --check` 验证零差异，再跑 `cargo test`。
5. `sync.md` 的版本标记改为 `1.1.0`。

---

## 五、整段不列出的 pi 功能（prux 无对应实现，且早于本区间）

以下在 changelog 里有条目，但 prux 没有对应功能、且该功能**不是** 1.0.2→1.1.0 新增，
按收录规则不展开（改动内容仅供将来接入时参考）：

- **Mistral**（`finish_reason: "error"` 重试、请求超时只作用于响应头）
- **Amazon Bedrock**（HTTP/2 pending stream、Converse reasoning effort、OpenAI on Bedrock 的 thinking level、
  prompt caching、pricing tiers）
- **Radius**（组织禁用模型不再列出、网关目录整体替换基线）
- **`openai-codex` provider**（originator / User-Agent 头可被 `models.json` 覆盖、
  `LoginOptions.agentName` 的 Codex originator 部分）
- **llama.cpp / systemone 原生决策模型**（见 2.19）
- **`packages/{durable,env,server,client,protocol,telemetry}`**：durable 的 `order` 破坏性变更、
  `packages/env` 首次发布（`RemoteExecutionEnv` + SSH bootstrap）、server / client / protocol / telemetry
  本区间无条目。prux 是单 crate 架构，均结构性不适用。
- **Node / npm 打包面**：`npm-shrinkwrap.json` 移除、`pi update` 的 managed 安装与版本清理、
  独立二进制不再加载 `.env`、`node --watch` 的图片 resize、pnpm 全局更新后的重启提示。
- **JS 类型层**：`AssistantMessageEventStream` 破坏性变更、`streamProxy()` 返回类型、
  faux provider 的 prompt-cache 估算、`models.generated.ts` 的 provider 重命名。
- **`showHardwareCursor` 只画终端光标 + `renderFakeCursor()` / `CURSOR_MARKER`**：
  该条在 coding-agent / tui 的 `[Unreleased]` 段（**晚于 1.1.0**），不在本区间；
  且 prux 本就用 `frame.set_cursor_position` 只画终端光标，本来就满足。
- **`Box.setPaddingX()` / `Text.setPaddingX()`**：属 tui 包组件 API；
  prux 无组件层，其效果由 2.11 承接。

---

## 六、落地检查清单

- [x] 决定 `SUPPORTED_PROVIDERS` 是否加 `azure`（2.18）——**第二轮已加**，同步范围 34 → 35 个 provider
- [x] `make sync-models` + `scripts/sync-models.py --check`（1.24 / 2.16 / 2.17 数据面）
- [x] 新增 `assets/changelog/v.1.1.0.md`，`Cargo.toml` version → `1.1.0`（`sync.md` 已是 `1.1.0`）
- [x] `RETRYABLE` 补 `server_busy` / `servers are currently busy`（1.1）
- [x] 按上下文钳制输出上限（1.18）
- [x] `--tools` 通配 + MCP 保留 + `+name`/`-name` + `--no-mcp` + `-x`（2.1 / 2.2 / 2.3）（`-xt` 两字符短名 clap 不支持，只加了 `-x`）
- [x] codemode：输出文件模块 + `image()` 落盘（2.4 / 2.21）、`read` 图片透传（2.5）、
      lockdown（2.6）、`console` 标记与输出分隔（2.7）、`classify` images（2.8）、
      guidelines 内联（2.22）
- [x] `agent_settled.aborted`（2.12）、`tool_execution_end.durationMs`（2.10）、
      `AssistantMessage.duration_ms`（2.10）
- [x] `Took` 用记录时长（1.5）
- [x] OAuth 刷新不因取消而丢 token（1.3）
- [x] MCP OAuth 可取消 + 15s 超时 + 会话关闭中止（1.7 / 2.14）
- [x] 切会话重置文本选择（1.9）
- [x] Termux 剪贴板读写（1.10）
- [x] 技能提示 `read` 隐藏时的降级措辞（1.6）
- [x] `Home`/`End` 与 `Ctrl+Home`/`Ctrl+End` 键位对齐（3.3）
- [x] Anthropic 登录端口占用行为核对（1.2）
- [x] 终端消失错误识别（1.17）
- [x] （可选）`openai-decisions`（2.17）——**第四轮已做**；OSC 7501 程序状态（2.13）仍未做
- [x] `outputPad` 设置项（2.11）——**有意跳过**，理由见第九节；prux 的 transcript 缩进硬编码 1 列、不可配置

---

## 八、第二轮实施记录（bug 修复收尾）

本轮把第一节剩余可自主落地的 bug 全部修完，并完成数据面同步。

| # | 项 | 改动位置 | 关键测试 |
|---|---|---|---|
| 1.18 | 输出上限按上下文钳制 | `core/provider.rs`（`clamp_max_tokens_to_context`）+ 新增 `core/provider/estimate.rs`（token 估算真源，`core/compaction` 改为转发） | `max_tokens_is_clamped_to_context_room`、`clamp_keeps_explicit_no_limit`、`clamp_skips_context_room_when_window_unknown`、`declared_tools_and_system_prompt_count_toward_context` |
| 1.24 | 模型目录同步 | `assets/models/**`（源为本地 pi-ai 1.1.0，`cost.tiers` 由 27 条增至 189 条） | `scripts/sync-models.py --check` 零差异 |
| 3.3 | Home/End 键位对齐 | `core/keybindings.rs`、`modes/interactive/handlers.rs`、`assets/docs/{keybindings,tui}.md` | `message_scroll_bindings_default_to_ctrl_home_end`、`ctrl_home_scrolls_message_area_to_top`、`ctrl_end_scrolls_message_area_to_bottom` |

**实现要点**：

- **1.18**：钳制在 `stream_chat` 进入协议分发前生效，覆盖 agent 主循环、cache warmer 与 proxy 网关三条调用路径；
  生效上限取 `max_tokens.or(model.max_tokens)`，两者都为 `None` 时保持「请求体不带该字段」；
  `context_window == 0`（窗口未知）时只做 `>= 1` 的下限保护。
  与 pi 的口径差异：pi 用 `chars / 3.5` 启发式且只算 transcript，
  prux 用 tiktoken BPE 真实计数，并额外计入 system 提示与工具声明的 JSON。
  token 估算的实现在 `core/provider/estimate.rs`（provider 层），`core::compaction` 的
  `estimate_tokens` / `estimate_context_tokens` / `calculate_context_tokens` / `usable_usage`
  改为从 provider 转发，调用路径不变——否则要么重复一份算法，要么让最底层反向依赖中层。
- **1.24**：数据由 `make sync-models` 生成，源是本地 pi-ai 1.1.0 的 `dist/providers/data/`。
  本轮未加 `azure`（2.18 不在范围），同步范围仍是原 34 个 provider。
  **副作用**：`openai/gpt-6-luna`（`api: openai-decisions`）随目录进入——该协议未实现，
  它会被列为可用分类器，调用时返回 `provider openai does not support classification (api: openai-decisions)`
  的错误结果（不崩栈）。要真正可用需要 2.17；若不想列出，需在目录加载层过滤未实现 api 的条目。
- **3.3**：**破坏性键位变更**——`shift+home` / `shift+end` 不再滚动消息区，
  `ctrl+home` / `ctrl+end` 不再移动编辑器光标。用户在 `keybindings.json` 里显式绑过旧键位的需自行调整。

**验证**：`cargo test` 全绿（lib 3234 项 + 全部集成测试目标）；`cargo fmt --check` 通过。

**版本面**：`Cargo.toml` → `1.1.0`（`Cargo.lock` 同步），新增 `assets/changelog/v.1.1.0.md`。
changelog 正文不再沿用上一版的「产品介绍」格式，而是按本节已落地项写成变更记录
（新增 / 变更 / 修复 三段，双语）；既有约定是版本间正文逐字节相同（`v.1.0.0.md` 与 `v.1.0.2.md`），
本版有意打破，以让 `/changelog` 真的能回答「What's New」。


---

## 九、第三轮实施记录（新增功能，A 档完成）

本轮落地第二节里「有明确建议、可自主实现」的全部新增项，只留下 2.11 一项有意跳过。

| # | 项 | 改动位置 | 关键测试 |
|---|---|---|---|
| 2.1 | `--tools` 通配 | 新增 `core/tool_names.rs`（`wildcard_matches` / `matches_any`）；`ToolSelection::is_allowed` 与 `compose_tools` 的内置选择改为按 matcher 过滤**注册表**；`args.rs` 补 `-x` | `tool_selection_supports_wildcard_entries`、`core::tool_names::tests::*` |
| 2.2 | MCP 保留 + `--no-mcp` | `ToolSelection::{is_mcp_reserved, declares_to_model}`；`script_tools_for` 让保留的工具仍可脚本调；`extensions::mcp::{EXT, TOOL}` 公开 + `main.rs` 据此禁用 | `allowlist_keeps_mcp_tools_unless_pointed_at`、`gateway_tool_name_matches_core_constant` |
| 2.3 | `+name` / `-name` | `settings_manager::{is_tool_modifier, apply_tool_modifiers, get_tool_list_error}` 公开；`main.rs` 求值，`Agent::set_default_tool_modifiers` 记下修饰符供 `/reload` 重放 | `tool_list_validation_matches_pi`、`reload_reapplies_default_tool_modifiers`、`cli_args::parses_tool_options` |
| 2.4 / 2.21 | 输出文件模块 + `image()` 落盘 | 新增 `utils/output_files.rs`；`codemode::{save_images, spill_output}`；`bash` 的 `full_output` 改存 `(File, PathBuf)` | `utils::output_files::tests::*`、`script_images_are_spilled_with_a_path_label`、`script_images_report_save_failures_in_the_label` |
| 2.5 | `read` 结构化输出 | `tools/read.rs::{output_schema, with_read_output}`；`index.rs` 的 `read_def` 声明 schema | `read_image_structured_output_matches_the_schema`、`read_text_structured_output_is_the_text`、`script_value_exposes_structured_output` |
| 2.6 | sandbox lockdown | `glue.js` 移植 pi 的 `lockdown()`（含 `OVERRIDABLE` accessor 处理） | `builtins_are_locked_down`、`locked_builtins_still_allow_instance_overrides` |
| 2.7 | `console` 标记 + 输出分隔 | `ScriptOutput::Console`；`codemode::{format_output, join_adjacent_text}`；`description.rs` 的 INTRO 与 codemode 文档同步 | `format_output_labels_items_and_groups_console`、`join_adjacent_text_inserts_a_separator` |
| 2.8 | `classify` images | `ClassifierContext.images`；`provider::classify` 的能力检查（`assertClassifierInputSupported` 等价）；`codemode::classifier_context_arg`；`typesafe-system-one` 自己再拒一次 | `image_input_requires_a_model_that_accepts_images`、`classifier_context_shape_is_validated` |
| 2.10 | `durationMs` | `FinalizedToolCall.duration_ms` 进 `tool_execution_end`；`provider::stamp_stream_duration` 给流式 assistant 消息盖耗时 | `tool_execution_event_payloads_match_pi`、`stream_duration_stamping_matches_pi` |
| 2.12 | `agent_settled.aborted` | `emit_agent_settled` | `agent_settled_reports_whether_the_run_was_aborted` |
| 2.22 | codemode 内联 guidelines | `declarations::render_tool_sample` 加 `guidelines` 参数，两个调用点同步 | `render_tool_sample` 的两条断言 |

**与 pi 的差异（有意保留）**：

- **2.2**：prux 的 MCP 是单个聚合网关工具（`mcp`，模型经 `action` 调用），不是 pi 的逐服务器工具。
  因此「保留」表现为**不对模型声明、但仍在注册表里可被 codemode / 嵌套调用**；
  pi 的 `list_mcp_resources` 等资源工具在 prux 由 `mcp` 的 `resources` / `read_resource` action 承接。
  `--no-tools`（空 allowlist）与名字里出现 `mcp__` 前缀条目仍会挡掉 MCP（对齐 pi 的 `_allowlistFiltersMcp`）。
- **2.4**：pi 把图片路径提示插在各自图片**前面**；prux 的 `ToolResult` 只有「一段文本 + 附件数组」两个槽，
  提示因此统一追加在正文之后、附件之前。
- **2.6**：只移植了 `lockdown()`；宿主侧对畸形 payload 的**显式** `Result` 校验未做——prux 用进程内 rquickjs，
  畸形 payload 不会崩宿主（`finish()` 降级、`__settle` 失败走 `ErrorKind::Sandbox`），语义等价但错误文案不如 pi 明确。
- **2.7**：`==> text N/M <==` 的编号只算 `text()` 与顶层 `return` 项，`console.*` 归并进末尾一个 `<console_output>` 块。
  描述里只写一句概括，细则写进了 codemode 文档（`assets/docs/extensions/codemode.md`），
  否则 INTRO 的固定开销会顶破 `fixed_overhead_stays_small` 的预算（上限已按本轮新增量从 470 调到 530）。
- **2.10**：`AssistantMessage.durationMs` 不在各 provider 里填，而是由 `stream_chat` 统一盖
  （对齐 pi 把计时放在 `AssistantMessageEventStream` 的位置）；`timestamp` 早于本次流开始的消息不盖。
- **2.11（outputPad）**：**未做**。`outputPad` 在 pi 是 TUI 组件 API（`Box/Text.setPaddingX`）的产物，
  prux 没有组件层；落地要给出参数量已超长、多处 `#[allow(clippy::too_many_arguments)]` 的
  消息渲染函数链（`render_message` → `render_message_blocks` → `render_user` / `render_assistant` /
  `render_tool_block_inner` / `render_summary` …）各加一个参数，收益（缩进可配）与回归风险不成比例。
  **结论：prux 的 transcript 缩进硬编码为 1 列、不可配置**，`/settings` 里不提供该项。

**验证**：`cargo test` 全绿（lib 3261 项 + 全部集成测试目标）；`cargo fmt --check` 通过；
`cargo clippy --lib --tests` 只剩改动前就存在的 3 条 warning（`settings_manager` 1、`tasks/widget` 2）。

---

## 十、第四轮实施记录（B 档：新协议 / 新 provider）

本轮落地 2.17（OpenAI Decisions 分类协议）与 2.18（azure provider 接入）。B 档两项均已完成。

| # | 项 | 改动位置 | 关键测试 |
|---|---|---|---|
| 2.17 | `openai-decisions` 协议 | 新增 `core/provider/decisions.rs`；`provider::classify` 加分支；`retry::send_with_retry_ex` 支持 `no_retry_statuses`；`classifier::{parse_usage, token_count}` 提为 `pub(crate)` 复用 | `decisions::tests::maps_questions_to_decisions_types_and_parses_answers`、`images_become_one_user_message_with_data_urls`、`gateway_504_fails_without_retry` |
| 2.17 | ChatGPT 登录时隐藏分类器 | `model_resolver::classifier_hidden_by_chatgpt_sign_in`；`list_models_of_type` 与 `find_model_impl` 同一口径 | `openai_classifier_listing_follows_chatgpt_sign_in` |
| 2.18 | azure provider | 新增 `core/provider/azure.rs`；`SUPPORTED_PROVIDERS` / `auth::api_key_env_var` / `login_registry` 加 azure；`provider::stream_chat` 分发前替换 `base_url`；responses / completions 的 endpoint 与请求体 `model` 走 azure 解析 | `azure::tests::normalizes_azure_host_paths`、`base_url_prefers_env_then_resource_then_catalog`、`deployment_map_rewrites_only_azure_request_models`、`api_version_query_follows_env_and_provider`、`resolve_if_azure_passes_non_azure_through`、`azure_resolves_endpoint_and_deployment_for_both_apis` |
| 2.18 | 目录同步 | `assets/models/**` 新增 azure 的 45 个条目（44 个 responses + 1 个 completions） | `scripts/sync-models.py --check` 零差异 |

**行为/接口口径**：

- **2.17**：`input` 是 `state` 的 JSON 文本；带图片时改为一条 user 消息（`input_text` + 每个图片一个
  `input_image` 的 data URL，上限 128 张）。`bool` 问题线上转 `predicate`（Decisions 没有判据字段），
  `true` / `false` 两侧含义并入 `instructions`。`answers` 是数组、按 `name` 对号入座，与问题顺序无关。
- **2.17**：网关 504 **不重试**（重试同样的超长输入必然再撞），并给专属文案说明原因。
- **2.17**：`ClassifierContext::images` 与图片生成共用 `ImageContent`；出现非图片项按错误结果返回。
- **2.17（与 pi 的差异）**：prux 的 `questions` 是 `BTreeMap`，请求里按 id **字典序**下发
  （pi 用对象的插入顺序）。服务端按 `name` 匹配，不影响结果；测试按同一顺序断言。
- **2.18**：目录条目的 `baseUrl` 为空，请求前按 `AZURE_OPENAI_BASE_URL` →
  `AZURE_OPENAI_RESOURCE_NAME` → 目录 `baseUrl`（可由 `models.json` 覆盖）解析；
  Azure 主机上把缺省路径归一为 `/openai/v1`，非 Azure 主机（自建网关）只去尾斜杠。
- **2.18**：请求体的 `model` 发**部署名**（`AZURE_OPENAI_DEPLOYMENT_NAME_MAP` 的 `modelId=deployment`，
  未命中则用目录 id）；`model_id` 本身不变，响应里的模型比对仍按目录 id。
- **2.18**：新版 `v1` 接口不加 `api-version` 查询参数；`AZURE_OPENAI_API_VERSION` 设为别的值时才追加。
- **2.18（与 pi 的差异）**：prux 没有 pi 的三个 SDK 选项（`azureBaseUrl` / `azureResourceName` /
  `azureDeploymentName`），对应覆盖入口是环境变量与 `models.json` 的 `baseUrl`；
  解析失败的文案据此改写（不再提不存在的选项）。azure 只支持 API key，没有订阅登录。

**验证**：`cargo test` 全绿（lib 3271 项 + 19 个集成测试目标，含 `tests/`）；
`cargo fmt --check` 通过；`cargo clippy --lib --tests` 只剩改动前就存在的 3 条 warning
（`settings_manager` 1、`tasks/widget` 2）。
