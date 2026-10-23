## English

This release aligns prux with upstream pi 1.3.0.

### Added

- **`ssh-remote` extension: remote tools and port tunnels**: adds `remote_exec` (run a command on
  a remote host), `remote_read` / `remote_ls` / `remote_grep` (read remote files) and `remote_sync`
  (copy files or trees between local and remote, with `dry_run`), plus `/ssh` for target management
  (`/ssh help` prints the target syntax and every subcommand) and `/tunnel` for mapping a remote port onto `127.0.0.1`. Built-in tools are untouched: `read`,
  `write`, `edit`, `ls`, `find`, `grep`, `bash`, `@` completion and `!` commands keep operating on
  local files only, so nothing ends up half-redirected. Targets accept `-i`, `-p`, `-J` and
  `-o ProxyJump=` and resolve `~/.ssh/config` aliases; the outbound proxy comes from the extension's
  own `proxy` setting, not from environment variables. Authentication tries public keys
  first (`-i`, `IdentityFile`, then the default keys), then a password from `-o PasswordFile=`
  (0600), `PRUX_SSH_PASSWORD`, or a masked TUI prompt, falling back to keyboard-interactive for
  PAM-only servers; credentials stay in process memory and `/ssh forget` clears them. SSH agent is
  not supported. Host keys are remembered TOFU-style with a hard refusal when the fingerprint
  changes. Large files are read by line range instead of being downloaded whole, and command output
  is bounded per call (200 lines / 8 KiB) and per turn (32 KiB). Disabled by default, and the
  five tools are **deferred**: the model finds them through `tool_search` first and sees the full
  declaration from its next call onward, so the `tool-search` extension has to be enabled.
- **`/share` destination is configurable**: `settings.json` takes
  `sessionShareDestination` (`auto` / `gist` / `radius`), switchable from `/settings`. `auto` and
  `gist` create a private gist; `radius` fails with a clear message instead of silently falling back,
  because this build has no Radius upload.
- **`PRUX_CLIPBOARD_OSC52=1` also emits OSC 52** after a successful local clipboard write, so remote
  clients that leave no SSH markers (multiplexer attaches, remote IDE daemons) still receive the copy.
- **OpenRouter catalogs are personalized per key**: a refresh with credentials asks
  `GET /models/user` instead of the public catalog, keeps only the chat models that key may use and
  applies the key's context length, max output and prices.
- **Theme files are validated at startup**: a theme missing required color tokens, using unknown
  keys or an out-of-range palette index is reported with the full reason instead of being silently
  replaced by the built-in default.
- **codemode declarations tag constraints the TypeScript type cannot express**: properties with
  bounds, `pattern`, `format`, length or item limits, `uniqueItems`, a non-null `default` or
  `deprecated` get a `// @integer @minimum 0 @maximum 30000 @default 0` comment line. Reachable
  bounds are merged (the tightest wins), a nullable `anyOf` / `oneOf` is read from its non-null
  branch, and tags are dropped before a declaration over the size budget degrades to `unknown`.
- **Refreshed model catalog** from pi-ai 1.3.0: 102 models were added and 40 removed across
  `github-copilot`, `google`, `huggingface`, `meta`, `nvidia`, `opencode`, `opencode-go`,
  `openrouter` and `vercel-ai-gateway`. OpenCode Zen and OpenCode Go now use the
  `opencode.ai/inference` endpoints, `kimi-coding` speaks OpenAI Chat Completions at
  `https://api.kimi.com/coding/v1` (a `models.json` base URL override must include `/v1`),
  Xiaomi MiMo exposes only the `off` and `high` thinking levels, and Claude Haiku 5.5 / Opus 5
  support the `off` thinking level.
- **`--model <name>` resolves a model offered by several providers**: prux prefers the provider
  that has credentials, and otherwise reports the ambiguity instead of silently picking one by
  catalog order. `--models` and `enabledModels` add an entry for every provider that offers the
  model and warn that the name is ambiguous.

- **`/usage` shows provider quota usage**: enabling the `usage` extension adds `/usage`, an overlay
  listing the connected providers with their rate-limit windows (Claude's 5-hour / 7-day / model-scoped
  windows, ChatGPT's 5-hour / weekly windows, Copilot quotas, OpenRouter credits, Kimi and Z.AI Coding
  Plan windows) plus balances for DeepSeek and Moonshot. Each row draws a 24-cell bar with the used
  percentage and reset countdown. Providers that expose no quota endpoint are named in the last line.
  Reports are cached for 120 seconds; `--offline` skips the requests entirely. Off by default.

- **The `markit` extension reads documents as Markdown**: the `read_doc` tool (deferred, so it
  first has to be found through `tool_search`) takes one `path` and dispatches on the extension —
  `.pdf` through its text layer, `.docx` into headings / lists / tables, `.xlsx` / `.xls` / `.xlsb` /
  `.xlsm` / `.ods` into one Markdown table per sheet, `.pptx` into one section per slide, and
  `.epub` into chapters in spine order. Files above 32 MiB are refused before parsing, output is
  truncated like `read` (2000 lines / 50KB), and a document with no text layer says so instead of
  returning nothing. It replaces the PDF-only `read_pdf` tool. Off by default.

- **The `lsp` extension asks a language server for facts grep cannot give**: one `lsp` tool with
  `operation` = `diagnostics` / `definition` / `references` / `symbols`. Servers are discovered on
  `PATH` from the file extension (rust-analyzer, clangd, typescript-language-server, pyright,
  gopls, jdtls, ruby-lsp, lua-language-server, csharp-ls, intelephense), and each
  command + workspace root pair stays resident until the extension is disabled. Positions are
  1-based and counted in characters; the file is synced to the server before each query, so
  diagnostics cover edits that are on disk but not yet compiled. Off by default.

- **The `shake` extension drops heavy content on demand**: `/shake [elide|images|thinking]`
  queues a one-shot mark that is applied before the next request. `elide` (the default) replaces
  tool result text and oversized fenced / XML blocks with short placeholders while leaving the most
  recent ~4000 tokens alone, `images` strips image blocks, and `thinking` drops thinking blocks
  (the last assistant message keeps its own). Only the model context changes — session files, `/tree`
  and the transcript are untouched — and the chat area reports how many pieces were dropped and how
  many tokens were freed. Off by default.

- **`memory` keeps durable notes across sessions**: enabling the `memory` extension adds a
  `memory` tool (`remember` / `recall` / `search` / `list` / `forget` / `tag` / `link` /
  `related`) and injects a relevance-selected block of remembered notes before each request. Recall is
  lexical (BM25), memories live in `agent_dir()/extensions/memory/` split into a global file and
  one per working directory, and every 12 turns a single one-shot request extracts new entries
  from the conversation. `--offline` skips extraction. Off by default.

- **Extensions can wait for the user mid-call**: a new `BeforeToolCallAsync` hook lets an extension
  gate a tool call and `await` the answer instead of having to decide synchronously. The matching
  `ask_prompt(title, options, timeout)` queues the usual select panel and waits for the pick — it
  holds no thread, is cancelled by dropping the future (Esc abort), and returns `None` when there is
  no interactive loop. A replaced panel resolves the waiting side with `None` (the event loop
  reconciles `PanelKind::Custom(id)` every frame), `ask_pending()` lets a caller avoid stealing
  another prompt's panel, and `submit_ui_choice` no longer fires a follow-up prompt when it answers a
  waiter.
- **`/permissions` gates what the agent may do**: enabling the `permissions` extension adds a
  deterministic gate in front of every tool call. Policy is layered — `path` (cross-cutting file
  patterns), `external_directory` (CWD boundary), per-tool patterns and `bash` command patterns —
  and composes most-restrictive-wins (`deny` > `ask` > `allow`), with last-match-wins inside each
  pattern map. Values are `allow` / `ask` / `deny` or `{action, reason}`; `path` and
  `external_directory` have directional variants (`*_read` / `*_write`). Hitting `ask` opens a
  confirmation panel (`Approve once` / `Approve for this session` / `Deny`); a panel that gets no
  answer (Esc, ten-minute timeout, non-interactive run, or another prompt already waiting) fails
  closed, `/permissions allow <surface> <pattern>` is the non-interactive escape hatch, and a session
  grant never overrides a `deny`. Bash commands are split into statements and each one is evaluated,
  including commands inside `$(…)` / backticks / subshells, paths after a literal `cd`, and redirect
  targets; opaque wrappers (`bash -c`, `eval`, `sudo`, `xargs`, `find -exec`, interpreter `-c`) and
  commands the parser could not finish are refused as `ask`. Config lives in
  `agent_dir()/extensions/permissions/config.json`, plus a project config that is only loaded when the
  project is trusted. Decisions go to a review log under `agent_dir()/extensions/permissions/logs/`.
  `/permissions settings` opens the extension settings panel (`permissionReviewLog` / `yoloMode`).
  Off by default. See `docs/extensions/permissions.md`.
- **`vet` closes the loop on "is it really done?"**: enabling the `vet` extension runs the
  project's own checks (test / lint / typecheck) right before the main agent settles. A blocking
  failure is injected as a user message — check names, commands, exit codes and the tail of each
  output — and the turn resumes so the agent keeps fixing until everything passes. Three conditions
  end the loop: `maxIterations` resume attempts (`0` = report only), an identical failure
  fingerprint (same check, same output), or a new user prompt. Checks come from
  `agent_dir()/extensions/vet.json` plus a trusted project's `.prux/extensions/vet.json` (keys
  override, `checks` replaces wholesale); an untrusted project's config is never read, so a cloned
  repo cannot make `prux` execute commands. `requireEdits` (default on) skips turns that changed no
  files, `optional` checks report without blocking, a per-check `timeoutSecs` kills runaway
  processes, and output is truncated to its tail (4000 chars per check, 12000 per report).
  `/vet settings|run|status|detect|add|remove|reset` manage it, `/vet detect [--write]` recognizes Cargo,
  npm/pnpm/yarn/bun, make/just, pytest/ruff, Go and Deno projects, and the settings panel switches
  `autoRun` / `maxIterations` / `timeout` / `announce` / `requireEdits`. Off by default. See
  `docs/extensions/vet.md`.

- **New `computer` extension (off by default)**: 14 `computer_*` tools that drive the real desktop —
  screenshots, mouse/keyboard, window listing and focusing, accessibility-tree reads and actions,
  clipboard, paste-with-restore, app launch and an emergency stop. Enabling the extension in
  `/extension` is the only switch; the whole family is declared as deferred tools, so the model loads
  it through `tool_search` and the schemas cost no context until used. `/computer` reports the backend,
  displays and which tools are loaded. Screenshots return a `frame` id so
  coordinates can be given as pixels *in the image* and are projected back (a frame whose window has
  moved is refused). Linux supports Wayland only (`grim` or `xcap` for capture, `swaymsg` for windows,
  `enigo` virtual keyboard/pointer, AT-SPI2 for accessibility); Windows and macOS use `xcap` + `enigo`
  and read the accessibility tree through UI Automation (Windows) and AXUIElement (macOS). Subagents cannot call it.

### Changed

- **`read` shows its line range as `:start-end`** in the transcript and in HTML exports, the same
  shape the session viewer uses. Numeric strings and `null` are accepted, so `offset: "5"` now shows
  a range instead of nothing. `read`'s `offset` is documented as 1-based.
- **Switching models no longer feeds the previous model's reasoning to the new model.** Thinking
  blocks produced by a different provider, API or model are dropped instead of being sent as plain
  text, so the new model no longer treats another model's reasoning as part of its own reply.
- **codemode's tool description** now states that a tool declaring an output schema resolves with
  its value even when the call fails, instead of claiming every failure rejects.
- **`tool_search` loads up to 15 matches by default** (was 8), enough to pick up a whole deferred
  family in one call.

### Fixed

- **A stream cut off before `finish_reason` now fails instead of ending the turn silently**: when a
  provider that reports `finish_reason` closes the stream mid-answer, the partial text and tool
  arguments are kept, but the turn ends with an error saying the response was truncated instead of
  being reported as a normal stop.
- **A final SSE frame without a trailing newline is no longer dropped**: such a frame (often the one
  carrying `finish_reason`) used to be lost when the connection closed right after it.

- **Three more transient provider errors are retried**: `The upstream provider returned an error`,
  `The service is temporarily unavailable` (which also made the first compaction attempt fail) and
  the gateway's `Unable to forward upstream stream`.
- **Tool-call arguments with an invalid `\u` escape** (for example `\usepackage`) no longer lose the
  field and everything after it: the escape is repaired and kept as literal text.
- **Malformed tool-call arguments fail instead of running partially**: a call whose final arguments
  are not valid JSON no longer runs with only the fields parsed before the error. It fails
  validation and the model is shown the raw arguments it sent.
- **Anthropic no longer rejects the request with a 400 when an error tool result contains images**:
  the images are attached after the tool results of the turn, in a block naming the tool call, so
  the model still sees them.
- **Thinking is no longer rendered after the answer** when a proxy (such as LiteLLM) sends reasoning
  and content in the same stream chunk.
- **Gemini 4 and later models send `thinkingLevel`** instead of `thinkingBudget`, so Gemini 4 Argon
  accepts the request and the selected thinking level applies.
- **Prompt templates keep empty quoted arguments** (`""` or `''`). Previously they were dropped,
  which shifted later positional arguments and prevented `${N:-default}` from applying.
- **Resource filters in the `skills` setting expand `~`**, so entries such as `!~/skills/pdf`,
  `+~/x` and `-~/x` match again.
- **Resumed sessions restore their active tools.** `--tools +name` / `-name` entries now apply on
  top of the tools a session activated, instead of the session falling back to the default set;
  an explicit `--tools` list, `--no-tools` and `--no-builtin-tools` still select tools outright.
- **Compaction, branch summaries and bug reports no longer include thinking** in the summarized
  conversation. Replayed thinking is often not in the live context, so including it could make the
  request larger than the context it summarizes and fail.
- **codemode fails a `// @options:` line that is not the first line** of the script, before the
  script runs. Such a line used to be ignored, so `timeout_ms` silently never applied.
- **Key bindings named `+` or `ctrl++` work** instead of being silently ignored.
- **codemode stops an awaiting loop** (`while (true) await null`) at the same interrupt points as a
  synchronous loop.
- **The `kimi-coding` login entry points at `https://api.kimi.com/coding/v1`**, matching the base
  URL the model catalog uses.
- **`keybindings.json` ignores its `$schema` reference** instead of treating it as an action entry.
- **`computer_launch` no longer writes the launched app's output into the TUI**: stdin is
  `/dev/null`, stdout and stderr go to a log file in the temp directory (path in the result) and,
  on Unix, the app gets its own process group, so a startup banner or a driver warning such as
  Chromium's "`--ozone-platform=wayland` is not compatible with Vulkan" no longer smears over the
  interface. On Linux the window is also found by process group now, so apps that hand their window
  to a forked process (Brave, Chrome) are recognised instead of waiting out the timeout as if no
  window had appeared. When the window does not appear, the result ends with the tail of that log.
- **`settings.json` now carries a `$schema` reference** (`https://pi.dev/schemas/settings.schema.json`)
  for editor completion and validation, and the packaged themes point their `$schema` at
  `https://pi.dev/schemas/theme.schema.json`. prux never reads the key; a value you set yourself is kept.
- **`computer_*` input injection reuses one connection instead of opening one per call**: every
  click, key and text run used to rebuild `enigo`, after which the compositor started rejecting new
  connections and the tools failed with `could not open an input channel: no successful connection`.
  A failed operation now drops the shared connection so a compositor restart recovers, and the error
  names the real cause: either the Wayland socket cannot be opened (with the OS error) or the
  compositor exposes neither `zwp_virtual_keyboard_v1` nor `zwlr_virtual_pointer_v1`.

## 中文

本版对齐上游 pi 1.3.0。

### 新增

- **新增 `ssh-remote` 扩展：远程工具族与端口隧道**：提供 `remote_exec`（在远端跑命令）、
  `remote_read` / `remote_ls` / `remote_grep`（读远端文件）、`remote_sync`（本地↔远端复制文件或
  目录树，支持 `dry_run`），以及 `/ssh` 目标管理（`/ssh help` 打印目标写法与全部子命令）与 `/tunnel` 端口映射。**不改写任何内置工具**：
  `read` / `write` / `edit` / `ls` / `find` / `grep` / `bash`、`@` 补全与 `!` 命令始终只操作本地，
  不会出现「一半穿透、一半不穿透」。目标支持 `-i`、`-p`、`-J`、`-o ProxyJump=`，会解析
  `~/.ssh/config` 别名；出站代理只认扩展自己配置里的 `proxy`，不读环境变量。认证先试公钥（`-i`、`IdentityFile`、再退到默认密钥），
  失败后取密码（`-o PasswordFile=` 的 0600 文件、`PRUX_SSH_PASSWORD`、或 TUI 掩码输入），对只开
  keyboard-interactive 的服务器自动用同一密码重试；凭据只留在进程内存，`/ssh forget` 清除。
  不支持 SSH agent。主机密钥按 TOFU 记录，指纹变化时直接拒绝。大文件按行范围读取而不是整份
  下载，命令输出有单次（200 行 / 8 KiB）与每轮（32 KiB）两级预算。**默认不启用**；五个工具都是
  延迟工具，模型先用 `tool_search` 检索（`query` 如 `ssh remote`）才能发现，下一次调用起完整声明可见，
  因此需要 `tool-search` 扩展处于启用状态。
- **`/share` 的上传目标可配置**：`settings.json` 新增 `sessionShareDestination`（`auto` / `gist` /
  `radius`），在 `/settings` 里可切换。`auto` 与 `gist` 建私密 gist；本版没有 Radius 上传，
  `radius` 直接给出明确报错，而不是静默降级成 gist。
- **设 `PRUX_CLIPBOARD_OSC52=1` 时追加一次 OSC 52**：本地剪贴板写入成功后仍会向终端发一次，
  让多路复用 attach、远端 IDE daemon 这类不带 SSH 标记的客户端也能拿到复制内容。
- **OpenRouter 目录按 key 定制**：持凭据刷新时改打 `GET /models/user`，只保留该 key 可用的 chat
  模型，并用该 key 的上下文长度、最大输出与价格覆盖目录。
- **启动时校验主题文件**：缺必需颜色 token、含未知键或越界色号的主题会逐条报出完整原因，
  而不是静默回退内置默认。
- **codemode 声明会标注类型表达不了的约束**：带界限、`pattern`、`format`、长度/数量限制、
  `uniqueItems`、非 null `default` 或 `deprecated` 的属性会多出一行
  `// @integer @minimum 0 @maximum 30000 @default 0` 注释。同时成立的界限会合并（取最紧的那个），
  可空的 `anyOf` / `oneOf` 取其非 null 分支，超预算时先去掉标签再降级为 `unknown`。
- **模型目录刷新**（来自 pi-ai 1.3.0）：新增 102 个模型、移除 40 个，涉及 `github-copilot`、
  `google`、`huggingface`、`meta`、`nvidia`、`opencode`、`opencode-go`、`openrouter`、
  `vercel-ai-gateway`。OpenCode Zen / Go 改用 `opencode.ai/inference` 端点；`kimi-coding`
  走 OpenAI Chat Completions（`https://api.kimi.com/coding/v1`，`models.json` 覆盖 base URL 时必须带
  `/v1`）；Xiaomi MiMo 只暴露 `off` / `high` 两档 thinking；Claude Haiku 5.5 / Opus 5 支持 `off` 档。
- **`--model <名字>` 能解析被多个 provider 提供的模型**：优先选有凭据的 provider，否则直接报歧义，
  不再按目录顺序静默挑一个。`--models` 与 `enabledModels` 会为每个提供该模型的 provider 各加一条，
  并提示该名字有歧义。

- **新增 `/usage` provider 配额面板**：开启 `usage` 扩展后可用 `/usage` 查看各 provider 的
  限额窗口（Claude 的 5 小时 / 7 天 / 模型周窗口、ChatGPT 的 5 小时 / 周窗口、Copilot 各功能配额、
  OpenRouter 额度、Kimi 与 Z.AI Coding Plan 窗口）以及 DeepSeek / Moonshot 的余额。
  每行是 24 格用量条 + 百分比 + 重置倒计时；没有配额端点的 provider 由末行点名。
  报告缓存 120 秒，`--offline` 不发请求。默认不启用。

- **新增 `markit` 文档读取扩展（取代只读 PDF 的 `pdf` 扩展）**：`read_doc` 工具（延迟工具，
  需先用 `tool_search` 载入）只吃一个 `path`，按扩展名分派——`.pdf` 抽文本层，`.docx` 还原
  标题 / 列表 / 表格，`.xlsx` / `.xls` / `.xlsb` / `.xlsm` / `.ods` 每个工作表一段 Markdown 表格，
  `.pptx` 每张幻灯片一节，`.epub` 按 spine 顺序拼接各章。超过 32 MiB 的文件在解析前拒绝，
  输出按 `read` 的口径截断（2000 行 / 50KB）；没有文本层的文档明确说明而不是返回空。
  默认不启用。

- **新增 `lsp` 语言服务器查询扩展**：`lsp` 工具按 `operation` 查 `diagnostics` / `definition` /
  `references` / `symbols`。服务器按文件扩展名在 `$PATH` 里找（rust-analyzer、clangd、
  typescript-language-server、pyright、gopls、jdtls、ruby-lsp、lua-language-server、
  csharp-ls、intelephense），每个「命令 + 工作区根」组合常驻复用，关闭扩展时一并退出。
  行列参数是 1-based、按字符计；每次查询前把磁盘上的当前内容同步给服务器，
  因此诊断能覆盖已写入但还没编译的改动。默认不启用。

- **新增 `shake` 手动丢弃重内容扩展**：`/shake [elide|images|thinking]` 只排一个**一次性标记**，
  在下一轮请求前生效。`elide`（默认）把工具结果正文与超大的围栏代码块 / XML 块换成短占位符，
  末尾约 4000 token 保持不动；`images` 丢掉图片块；`thinking` 丢掉思考块（最后一条 assistant 的保留）。
  只改模型上下文，不动会话文件，`/tree` 与聊天区历史照旧；实际丢掉的条数与释放的 token 会在聊天区回报。
  默认不启用。

- **新增 `memory` 跨会话记忆**：开启 `memory` 扩展后提供 `memory` 工具
  （`remember` / `recall` / `search` / `list` / `forget` / `tag` / `link` / `related`），
  并在每次请求前按当前输入注入一段按相关度挑选的记忆。召回是词法的（BM25），
  记忆落盘在 `agent_dir()/extensions/memory/`（全局一份 + 每个工作目录一份），
  每 12 轮用一次独立请求从对话里抽取新条目；`--offline` 跳过抽取。默认不启用。

- **扩展可以在工具调用中途等用户作答**：新增 `BeforeToolCallAsync` 钩子，扩展可以一边门控工具调用
  一边 `await` 用户的选择，而不必同步地当场下结论。配套的 `ask_prompt(title, options, timeout)`
  复用同一个选择面板并等待作答——不占线程，回合被 abort（future 被 drop）即取消，没有交互事件循环
  时返回 `None`。面板被替换时等待方会收到 `None`（事件循环每帧对账 `PanelKind::Custom(id)`）；
  `ask_pending()` 让调用方避开「顶掉别人的面板」；`submit_ui_choice` 在答复等待者时不再触发
  follow-up prompt。
- **`/permissions` 给模型的每次工具调用加一道确定性门控**：开启 `permissions` 扩展后，工具调用
  在执行前先过策略。策略分四层——`path`（横切文件模式）、`external_directory`（工作目录边界）、
  按工具的模式与 `bash` 命令模式——组合规则是 most-restrictive-wins（`deny` > `ask` > `allow`），
  单张模式表内是 last-match-wins。值可以是 `allow` / `ask` / `deny`，或带理由的
  `{action, reason}`；`path` 与 `external_directory` 还有方向变体（`*_read` / `*_write`）。
  命中 `ask` 会弹确认面板（`Approve once` / `Approve for this session` / `Deny`）；拿不到答复
  （按 Esc、10 分钟超时、非交互运行、或已有别的确认在等）一律按拒绝处理，
  `/permissions allow <surface> <pattern>` 是非交互场景的兜底，且会话放行**不会**覆盖 `deny`。
  bash 命令按语句切分逐条判定，包括 `$(…)` / 反引号 / 子 shell 里的命令、字面 `cd` 之后的路径
  与重定向目标；看不透的包装（`bash -c`、`eval`、`sudo`、`xargs`、`find -exec`、解释器 `-c`）
  与没能解析完的命令一律按 `ask` 拦下。配置在 `agent_dir()/extensions/permissions/config.json`，
  另有项目配置（仅在项目受信任时加载）；判定结果写进
  `agent_dir()/extensions/permissions/logs/` 下的 review log。`/permissions settings` 打开扩展设置面板
  （`permissionReviewLog` / `yoloMode`）。默认不启用。详见 `docs/extensions/permissions.md`。
- **`vet` 给「改完了」补上「检查过了」**：开启 `vet` 扩展后，主 agent 结算前会跑项目自己的检查
  （测试 / lint / 类型检查）。有阻塞失败时，把检查名、命令、退出码与输出尾部作为一条 user 消息
  注入上下文并续跑，让 agent 一直修到全绿。三种情况收手：用满 `maxIterations` 次
  （`0` = 只检查只报告）、失败指纹与上次相同（同一条检查同一种输出）、用户提交了新输入。
  检查来自 `agent_dir()/extensions/vet.json` 与受信项目的 `.prux/extensions/vet.json`
  （键级覆盖、`checks` 整体替换）；未受信项目的配置根本不读，一个陌生仓库没法让 `prux` 执行命令。
  `requireEdits`（缺省开）跳过没改过文件的回合，`optional` 检查只报告不阻止收工，单条检查的
  `timeoutSecs` 会连同子进程一起杀掉超时的检查，输出只保留尾部（单条 4000 字符、整份报告
  12000 字符）。`/vet settings|run|status|detect|add|remove|reset` 管理配置，`/vet detect [--write]`
  识别 Cargo、npm/pnpm/yarn/bun、make/just、pytest/ruff、Go、Deno 项目，设置面板可调
  `autoRun` / `maxIterations` / `timeout` / `announce` / `requireEdits`。默认不启用。
  详见 `docs/extensions/vet.md`。

- **新增 `computer` 扩展（默认关闭）**：14 个 `computer_*` 工具操作真实桌面——截图、鼠标键盘、
  窗口列表与置前、无障碍树读取与控件动作、剪贴板、粘贴后还原、启动应用、急停。在 `/extension`
  面板里启用就是唯一的开关；整族工具声明为延迟工具，模型经 `tool_search` 载入后才进工具表，
  未使用时不占上下文。`/computer` 显示后端、显示器与已载入的工具。截图返回 `frame` id，坐标可以按图内像素给出并由扩展投影
  （帧对应的窗口移动过后拒绝执行）。Linux 只支持 Wayland（`grim` 或 `xcap` 截图、`swaymsg` 列窗口、
  `enigo` 虚拟键鼠、AT-SPI2 读无障碍）；Windows / macOS 用 `xcap` + `enigo`，无障碍分别走 UI Automation 与 AXUIElement。
  子代理不可调用。

### 变更

- **`read` 的行范围标签统一为 `:start-end`**：对话区与 HTML 导出都按这个形状显示（与 session
  视图一致）。数字字符串与 `null` 也能解析，`offset: "5"` 不再什么都不显示；`read` 的
  `offset` 文档改为 1 起。
- **切换模型不再把上一个模型的推理喂给新模型**：其它 provider / API / 模型产生的 thinking 块被丢弃，
  而不是当普通文本发出去，新模型不会再把他人的推理当成自己回答的一部分。
- **codemode 的工具描述**改为说明「声明了 output schema 的工具失败时返回自己的值」，
  不再声称所有失败都抛错。
- **`tool_search` 单次默认最多激活 15 个工具**（原为 8），足以一次载入一整族延迟工具。

### 修复

- **流在 `finish_reason` 之前被抳断时报错，不再静默结束回合**：会报告 `finish_reason` 的 provider
  在中途关闭连接时，已收到的文本与工具参数仍会保留，但本回合以「响应被截断」的错误结束，
  不再谎报成正常收尾。
- **末尾不带换行的 SSE 帧不再被丢掉**：这类帧（常常正是携带 `finish_reason` 的那一帧）
  此前会随连接关闭一起丢失。

- **三条瞬时 provider 错误现在会重试**：`The upstream provider returned an error`、
  `The service is temporarily unavailable`（此前还会让压缩首次尝试就失败）、以及网关的
  `Unable to forward upstream stream`。
- **工具参数里的非法 `\u` 转义**（如 `\usepackage`）不再丢掉该字段及其后的全部内容：
  转义会被修复并保留为字面文本。
- **参数畸形时不再「部分执行」**：最终参数不是合法 JSON 的工具调用，不再拿错误前解析出的字段执行，
  而是在校验阶段失败，并把模型发来的原文回给它。
- **错误工具结果带图片时 Anthropic 不再回 400**：图片被挪到本回合全部 tool result 之后，
  并有一段点名该工具调用的说明，模型仍能看到它们。
- **代理（如 LiteLLM）在同一个 chunk 里同时给出推理与正文时，思考不再渲染在答案后面。**
- **Gemini 4 及以后发 `thinkingLevel`**，不再发 `thinkingBudget`：Gemini 4 Argon 的请求能被接受，
  选定的 thinking 级别也真正生效。
- **prompt 模板保留空引号参数**（`""` / `''`）：此前会被丢弃，导致后续位置参数整体前移、
  `${N:-default}` 的默认值落不上。
- **`skills` 设置的过滤条目会展开 `~`**：`!~/skills/pdf`、`+~/x`、`-~/x` 这类写法重新能匹配。
- **恢复会话时会装回会话的 active tools**：`--tools +name` / `-name` 作用在其之上，
  不再退回默认工具集；显式 `--tools 名单`、`--no-tools`、`--no-builtin-tools` 仍然整体覆盖。
- **压缩、分支摘要与 bug report 的对话序列不再包含 thinking**：replay 出来的思考往往不在当前上下文里，
  带上它会让摘要请求比被摘要的上下文还大，反而失败。
- **codemode 的 `// @options:` 行不在首行时会在脚本运行前报错**：此前这类行被静默忽略，
  `timeout_ms` 从不生效。
- **键位 `+` 与 `ctrl++` 可用**，不再被静默忽略。
- **codemode 能停下 await 循环**（`while (true) await null`），与同步死循环在同一批中断点生效。
- **`kimi-coding` 的登录条目指向 `https://api.kimi.com/coding/v1`**，与模型目录用的 base URL 一致。
- **`keybindings.json` 忽略 `$schema` 引用**，不再把它当成一个动作条目。
- **`computer_launch` 启动的应用不再把输出写到界面上**：标准输入接 `/dev/null`，标准输出与错误写进
  系统临时目录的日志（路径在回执里），Unix 上另起进程组，所以启动横幅或 Chromium 那类
  “`--ozone-platform=wayland` 与 Vulkan 不兼容”的驱动告警不会再糊在 TUI 上；窗口没出现时，
  回执末尾直接附上日志尾部。Linux 上还按进程组认回窗口，所以把窗口交给派生进程的应用
  （Brave、Chrome）能被认出来，不再白等到超时、报成“没有窗口”。
- **`settings.json` 会写入 `$schema` 引用**（`https://pi.dev/schemas/settings.schema.json`）供编辑器补全与校验，
  内置主题的 `$schema` 也指向 `https://pi.dev/schemas/theme.schema.json`。prux 自身不读取该键，
  你自己改过的值会保留。
- **TUI 期间进程的 stderr 改道到 `agent_dir()/logs/tui-stderr.log`**：`libxkbcommon` 这类
  进程内 C 库不受子进程管道约束，解析合成器发来的 keymap 失败时直接往 stderr 打
  `xkbcommon: ERROR: xkb_keymap_new_from_buffer: unsupported keymap format`，一次输入注入就能糊满整屏。
  现在这些告警（含漏网的子进程输出）落日志文件、退出时恢复正常 stderr；日志超过 1 MiB 下次启动清空重写。
  外部编辑器这类本来就要终端的程序仍能看到真实 stderr。
- **`computer_*` 的输入注入改为复用同一条连接**：此前每次点击、按键、输入文本都重新建立 `enigo`
  连接，之后合成器开始拒绝新建连接，工具只会报 `could not open an input channel: no successful
  connection`。现在操作失败即丢弃这条连接，合成器重启后能自愈；错误文案也直接点明真因——
  Wayland socket 打不开（附系统错误），或合成器既不提供 `zwp_virtual_keyboard_v1`
  也不提供 `zwlr_virtual_pointer_v1`。
