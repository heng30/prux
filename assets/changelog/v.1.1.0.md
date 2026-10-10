## English

This release aligns prux with upstream pi 1.1.0.

### Added

- **Refreshed model catalog** from pi-ai 1.1.0: Claude Haiku 5.5 is now available, and
  prompt-length pricing tiers were added for OpenCode, OpenRouter, Vercel AI Gateway, Google and
  MiniMax — long-prompt requests are no longer undercharged.
- **`--tools` accepts patterns and add/remove entries.** Entries may use `*` (for example
  `--tools read,codemode,'mcp__radius__*'`), and an all-`+name` / `-name` list adds or removes from
  the default selection instead of replacing it. `--tools` also keeps MCP tools unless an entry
  starts with `mcp__`, `--no-mcp` turns built-in MCP support off entirely, and `-x` is the short
  form of `--exclude-tools`.
- **codemode script output is easier to read.** `image()` saves each image to an owner-only
  (`0o600`) temp file and the result names its path; several text items are numbered
  (`==> text N/M <==`) and `console.*` lines are grouped into one `<console_output>` block.
- **`read` returns structured output**, so codemode scripts can show an image a nested `read`
  returned instead of only its text.
- **The codemode sandbox freezes the built-ins** before a script runs, so a script that patches a
  prototype (for example `Array.prototype.toJSON`) can no longer corrupt the host-visible result.
  A payload that still fails to decode now fails the run as a sandbox error (naming what broke)
  instead of silently dropping the script's result.
- **MCP OAuth dynamic registration sends `application_type` (MCP SEP-837)**: it is derived from the
  redirect URI — a custom scheme or a loopback host is `native`, anything else is `web` — and
  `oauth.applicationType` overrides it.
- **codemode tool declarations include each tool's prompt guidelines**, so `ALL_TOOLS` and
  `describeTool()` show the same guidance the system prompt shows.
- **`models.classify()` accepts `images`** (rejected with an error result when the model does not
  take image input), `tool_execution_end` carries `durationMs`, streaming assistant messages carry
  `durationMs`, and `agent_settled` carries `aborted`.
- **Azure is available as a provider (`azure`)**, serving both the Responses API and Foundry Chat
  Completions. The endpoint is resolved per request from `AZURE_OPENAI_BASE_URL` /
  `AZURE_OPENAI_RESOURCE_NAME`, and requests send the deployment name when
  `AZURE_OPENAI_DEPLOYMENT_NAME_MAP` maps one.
- **The OpenAI Decisions classifier protocol (`openai-decisions`) is implemented**, so
  `openai/gpt-6-luna` can be used for classification. Sign in with ChatGPT does not list it:
  that token is rejected by the Decisions API.
- **prux reports its state to the terminal** with the Program Status Protocol (OSC 7501): terminals
  and agent dashboards that answer the protocol's support query see `working`, `blocked` (an
  extension dialog or login is waiting), `done`, `error`, or `idle`. Reports hold only the session
  name, dialog titles, and the first line of errors — never prompts or model output.
  `PRUX_PROGRAM_STATUS=1` reports without asking, `=0` turns it off.
- **The `subagent` extension can run autoresearch jobs** with
  `/autoresearch <goal> [--iterations N]`. Every iteration runs the same prompt in the same forked
  sub-agent session — the first turn forks the current conversation, later turns resume it — and the
  job stops when the answer ends with `[goal-complete]` or the iteration limit is reached. Four jobs
  run at a time, `/autoresearch` lists them, `/autoresearch stop <id>` stops one, and a job keeps
  running when you press `Esc` (stop it explicitly).

- **`/cache-usage` reports prompt cache hit rates** from saved sessions, with three peer scopes:
  no argument reads the current session, `all` reads every saved session (all providers), and a
  provider name reports just that provider — which also reads every saved session, since a provider
  spans sessions while a single session often uses only one. The panel shows
  the total bar, a hit-rate sparkline over time, and per-provider / per-session bars; entries that
  forked sessions duplicate are counted once. It never makes network requests.
- **`/doctor` checks provider reachability** with one credential-free `HEAD` per provider
  (16 at a time, 10s each). Any HTTP response means the network path is fine — `2xx`, `401`, `403`
  and `405` are the expected answers, anything else is flagged `status not OK` — and only a failing
  request is listed as unreachable (`timeout` / `dns error` / `connect error`). `/doctor summary`
  prints the counts and the failures, the panel ends with an environment block (version, agent
  directory, proxy variables as set/unset only), and `--offline` skips the probes entirely.
- **`/btw` asks a read-only side question and `/recap` shows a one-line session summary.** `/btw`
  answers in a panel from a short excerpt through one independent request — nothing is added to the
  conversation — while `/recap [now|on|off]` puts a one-line summary in the dock and refreshes it
  after 5 minutes idle. They are separate extensions on purpose: `/btw` only acts when you ask,
  `/recap` spends tokens on its own.
- **These four new extensions are disabled by default** and live in the `Dev` and `Creator`
  extension modes: `cache-usage`, `doctor`, `btw`, `recap` (turn them on in the `/extension` panel).

### Changed

- **Output limits are clamped to the context window.** Before a request is sent, `max_tokens` is
  capped at `context window − estimated context − 4096`, so requests that would exceed the window
  are no longer rejected outright. Estimation uses tiktoken BPE counts and includes the system
  prompt and tool declarations.
- **`Home` / `End` only move the editor cursor; transcript top/bottom moved to
  `Ctrl+Home` / `Ctrl+End`.** Breaking change: `Shift+Home` / `Shift+End` no longer scroll, and
  `Ctrl+Home` / `Ctrl+End` no longer move the cursor.
- **Tool durations only measure the tool itself.** `durationMs` no longer includes
  `after_tool_call` hooks, and calls that never ran (unknown tool, blocked) no longer report a
  duration — they used to render as `Took 0.0s`.
- **Anthropic browser sign-in falls back to a free loopback port** when 53692 is occupied, instead
  of degrading to pasting the redirect URL.
- **MCP OAuth sign-in is cancellable** with `Esc` / `Ctrl+C` and is aborted on session shutdown;
  every request to the authorization server now times out after 15s (was 30s).
- **Skill hints degrade gracefully** when the `read` tool is hidden: they fall back to `bash`, or
  stop naming a tool, instead of being dropped or pointing at tools the model cannot use.
- **Mid-conversation system messages reach the models that accept them** (Anthropic, OpenAI Chat
  Completions, and OpenAI Responses). When a transcript carries a `system` message after the prompt,
  models whose catalog entry sets `compat.supportsMidConvoSystemMessages` get it natively; it is held
  back to just before the next non-tool message so a `tool_result` still follows its `tool_use`.
  For other models its text is merged into the leading system prompt instead of being dropped.
- **An autoresearch job now reports when it settles.** Finishing (or failing) posts a one-line
  notice with the iteration count, tokens, duration, goal and — for a failure — the reason, so a
  job no longer ends silently when its dock row disappears. The notice is user-facing only: the
  job still does not inject anything into the conversation, so read a round with
  `/agents result <id>`. Stopping a job by hand stays silent (the command already answers).

### Fixed

- `server_busy` / `servers are currently busy` provider errors are retried instead of ending the turn.
- A cancelled or superseded OAuth refresh no longer discards the rotated refresh token.
- Text selection is reset when switching sessions, forking, or rewinding, so stale selections no
  longer highlight unrelated text.
- Clipboard read/write works under Termux (`termux-clipboard-get` / `termux-clipboard-set`).
- A vanished terminal (`EIO` / `EPIPE` / `ENOTCONN` / `ENOTTY`) now exits quietly instead of
  crashing while reading input or rendering.
- Overlay content that is replaced wholesale (a `scanning…` placeholder giving way to a report)
  stays at the top instead of jumping to the end.
- The scrollbar of panel-style overlays is aligned with the content area (its track started one row
  too low), so the thumb reaches the first line.

## 中文

本版对齐上游 pi 1.1.0。

### 新增

- **模型目录刷新**（来自 pi-ai 1.1.0）：新增 Claude Haiku 5.5；OpenCode、OpenRouter、
  Vercel AI Gateway、Google、MiniMax 补上了「按 prompt 长度分档」的定价档，
  长 prompt 请求不再少算成本。
- **`--tools` 支持通配与增删条目**：条目可用 `*`（如 `--tools read,codemode,'mcp__radius__*'`）；
  全是 `+name` / `-name` 时改为在默认选择上增减，而不是整体替换。`--tools` 也不再顺带挡掉 MCP 工具
  （除非条目以 `mcp__` 开头），新增 `--no-mcp` 完全关闭内置 MCP 支持，`--exclude-tools` 多了 `-x` 短名。
- **codemode 脚本输出更好读**：`image()` 把每张图落到只有属主可读（`0o600`）的临时文件并在结果里
  给出路径；多个文本项带 `==> text N/M <==` 编号，`console.*` 的行汇总进一个 `<console_output>` 块。
- **`read` 返回结构化输出**：codemode 脚本可以直接展示嵌套 `read` 读到的图片，而不只是一段文本。
- **codemode 沙箱在脚本运行前冻结内建**：脚本再改原型（如 `Array.prototype.toJSON`）也不会污染
  宿主看到的结果；仍解不开的载荷现在会以沙箱错误失败（并指明坏在哪），而不是静默丢掉脚本结果。
- **MCP OAuth 动态注册上报 `application_type`（MCP SEP-837）**：按回调地址派生——自定义 scheme 或
  loopback 主机为 `native`，其余为 `web`；`oauth.applicationType` 可覆盖。
- **codemode 的工具声明内联各自的 prompt guidelines**：`ALL_TOOLS` 与 `describeTool()` 能看到
  与系统提示一致的信息。
- **`models.classify()` 接受 `images`**（模型不吃图片输入时返回错误结果）；工具事件
  `tool_execution_end` 带 `durationMs`，流式 assistant 消息带 `durationMs`，`agent_settled` 带 `aborted`。
- **新增 Azure provider（`azure`）**：同时服务 Responses API 与 Foundry Chat Completions；
  endpoint 每次请求按 `AZURE_OPENAI_BASE_URL` / `AZURE_OPENAI_RESOURCE_NAME` 解析，
  `AZURE_OPENAI_DEPLOYMENT_NAME_MAP` 命中时请求体发的是部署名。
- **新增 OpenAI Decisions 分类协议（`openai-decisions`）**：`openai/gpt-6-luna` 可用于分类。
  Sign in with ChatGPT 登录态下不会列出它——该 token 会被 Decisions API 拒绝。
- **prux 用 OSC 7501 程序状态协议向终端上报自身状态**：应答过支持查询的终端 / agent 面板
  能看到 `working`、`blocked`（扩展对话框或登录在等用户）、`done`、`error`、`idle`。
  上报只含会话名、对话框标题与错误首行，**绝不包含 prompt 或模型输出**。
  `PRUX_PROGRAM_STATUS=1` 不查询直接上报，`=0` 完全关闭。
- **`subagent` 扩展新增 autoresearch 作业**（`/autoresearch <目标> [--iterations N]`）：
  每轮都在同一个分叉出的子会话里跑同一条提示词（首轮 fork 当前对话，之后 resume 续跑），
  回答以 `[goal-complete]` 收尾或到达轮次上限即收工。同时最多 4 个作业，`/autoresearch` 列作业，
  `/autoresearch stop <id>` 停一个；作业不随 `Esc` 一起停（要停得显式停）。

- **新增 `/cache-usage`**：把会话历史里的 token 用量汇总成 prompt cache 命中率报告，
  范围三选一（同级）：空参 = 当前会话，`all` = 全部已存会话（所有 provider），
  `<provider>` = 该 provider——provider 是跨会话概念，因此也读全部已存会话。
  面板显示总量条形图、命中率趋势 sparkline
  与各 provider / 各会话分列；fork 会话里重复的条目只计一次。全程只读会话文件，不发网络请求。
- **新增 `/doctor`**：provider 网络可达性诊断。对每个 provider 发一个**不带凭据**的 `HEAD`
  （并发 16、单请求 10s）：**有 HTTP 响应就算网通**（`2xx` / `401` / `403` / `405` 是预期结果，
  其它状态码标 `status not OK`），只有请求本身失败才列入不可达（`timeout` / `dns error` /
  `connect error`）。`/doctor summary` 只给计数与失败清单，面板末尾的 environment 段列出程序
  版本、`agent_dir()`、探测目标数与代理变量的 set/unset（不回显值）；`--offline` 不发请求。
- **新增 `/btw` 与 `/recap`**：`/btw <问题>` 是只读旁问——答案渲染在输入区上方的面板里，
  上下文只有一小段摘录、走独立的一次性请求，**不进会话、不改上下文**；
  `/recap [now|on|off]` 在停靠面板里显示一行会话小结（≤120 字），空闲 5 分钟后自动重刷。
  两者**刻意拆成两个扩展**：旁问只在你开口时才有动作，小结会在后台自己花 token。
- **这四个新扩展都默认不启用**，且只在 `Dev` / `Creator` 扩展模式下可用：
  `cache-usage`、`doctor`、`btw`、`recap`（在 `/extension` 面板里开启）。

### 变更

- **输出上限按上下文窗口钳制**：请求发出前，`max_tokens` 会被压到
  「上下文窗口 − 已用上下文 − 4096 安全余量」以内，超窗请求不再被供应商直接拒绝。
  估算走 tiktoken BPE 真实计数，并计入系统提示与工具声明。
- **`Home` / `End` 只管编辑器光标；消息区顶/底改到 `Ctrl+Home` / `Ctrl+End`**。
  破坏性变更：`Shift+Home` / `Shift+End` 不再滚动，`Ctrl+Home` / `Ctrl+End` 不再移动光标。
- **工具耗时只计工具本身**：`durationMs` 不再包含 `after_tool_call` 钩子；
  未真正执行的调用（未知工具、被拦截）不再带耗时——此前会渲染成 `Took 0.0s`。
- **Anthropic 浏览器登录在 53692 被占用时改用空闲 loopback 端口**，
  不再退化为「粘贴 redirect URL」。
- **MCP OAuth 登录可取消**：`Esc` / `Ctrl+C` 与会话关闭都会中止登录；
  对授权服务器的每个请求超时从 30s 收紧到 15s。
- **技能提示按可用工具降级**：`read` 被隐藏时改用 `bash`，或干脆不点名具体工具，
  不再整段省略、也不再指向模型用不了的工具。
- **对话中途的系统消息能真正下发到接受它的模型**（Anthropic、OpenAI Chat Completions、
  OpenAI Responses）：transcript 里出现在首部提示词之后的
  `system` 消息，在目录带 `compat.supportsMidConvoSystemMessages` 的模型上按原生对话中途系统消息下发；
  它会缓发到下一条非 tool 消息之前，保证 `tool_result` 仍紧跟 `tool_use`。其它模型上这类消息的文本
  并入首部系统提示词，而不是被静默丢弃。
- **autoresearch 作业终结时会回报**：完成或失败都投一条通知（轮次、token、耗时、目标，
  失败还带原因），作业不再在 dock 行消失时无声收场。通知只给人看，仍**不**往主会话注入消息，
  看某一轮的结果走 `/agents result <id>`；用户自己停的作业不发（命令已经回过话）。

### 修复

- `server_busy` / `servers are currently busy` 这类供应商瞬时错误现在会重试，而不是直接结束回合。
- OAuth 刷新被取消或被替代时，不再丢掉轮转后的 refresh token。
- 切换会话、分支、回退时重置文本选择，旧选区不再命中无关文本。
- Termux 下剪贴板读写可用（`termux-clipboard-get` / `termux-clipboard-set`）。
- 终端消失（`EIO` / `EPIPE` / `ENOTCONN` / `ENOTTY`）时安静退出，不再在读取输入或渲染时报错崩栈。
- 覆盖层内容一次性换代（占位提示 → 正式内容）时停在开头，不再被“贴底跟随”拉到结尾。
- 面板式覆盖层的滚动条与内容区对齐（此前 track 起点偏低一行），thumb 拖到顶端就是第一行。
