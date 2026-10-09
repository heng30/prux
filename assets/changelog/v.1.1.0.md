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
- **codemode tool declarations include each tool's prompt guidelines**, so `ALL_TOOLS` and
  `describeTool()` show the same guidance the system prompt shows.
- **`models.classify()` accepts `images`** (rejected with an error result when the model does not
  take image input), `tool_execution_end` carries `durationMs`, streaming assistant messages carry
  `durationMs`, and `agent_settled` carries `aborted`.

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

### Fixed

- `server_busy` / `servers are currently busy` provider errors are retried instead of ending the turn.
- A cancelled or superseded OAuth refresh no longer discards the rotated refresh token.
- Text selection is reset when switching sessions, forking, or rewinding, so stale selections no
  longer highlight unrelated text.
- Clipboard read/write works under Termux (`termux-clipboard-get` / `termux-clipboard-set`).
- A vanished terminal (`EIO` / `EPIPE` / `ENOTCONN` / `ENOTTY`) now exits quietly instead of
  crashing while reading input or rendering.

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
  宿主看到的结果。
- **codemode 的工具声明内联各自的 prompt guidelines**：`ALL_TOOLS` 与 `describeTool()` 能看到
  与系统提示一致的信息。
- **`models.classify()` 接受 `images`**（模型不吃图片输入时返回错误结果）；工具事件
  `tool_execution_end` 带 `durationMs`，流式 assistant 消息带 `durationMs`，`agent_settled` 带 `aborted`。

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

### 修复

- `server_busy` / `servers are currently busy` 这类供应商瞬时错误现在会重试，而不是直接结束回合。
- OAuth 刷新被取消或被替代时，不再丢掉轮转后的 refresh token。
- 切换会话、分支、回退时重置文本选择，旧选区不再命中无关文本。
- Termux 下剪贴板读写可用（`termux-clipboard-get` / `termux-clipboard-set`）。
- 终端消失（`EIO` / `EPIPE` / `ENOTCONN` / `ENOTTY`）时安静退出，不再在读取输入或渲染时报错崩栈。
