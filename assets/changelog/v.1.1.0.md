## English

This release aligns prux with upstream pi 1.1.0.

### Added

- **Refreshed model catalog** from pi-ai 1.1.0: Claude Haiku 5.5 is now available, and
  prompt-length pricing tiers were added for OpenCode, OpenRouter, Vercel AI Gateway, Google and
  MiniMax — long-prompt requests are no longer undercharged.

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
