## English

### Added

- **`/backup` snapshots and restores your prux assets**: the new `backup` extension packs
  `agent_dir()`'s `settings.json`, `APPEND_SYSTEM.md`, `trust.json`, `themes/`, `skills/` and
  `extensions/` (extension configs, memory, MCP configs, workflows) into one zip — by default
  `agent_dir()/backups/prux-backup-<stamp>.zip`, or a folder you name. `manifest.json` goes in first
  with the prux version, the source folder and every file's size and sha256. Only `agent_dir()` is
  collected: a project's `.prux/` and `AGENTS.md` stay with the repository. Credentials never ship:
  `auth.json`, `sessions/`, `history/`, `logs/`, `bin/`, `docs/`, the rebuildable `models-store.json`
  and `recent-cwds.json`, the archives folder itself, and `extensions/worktrees`,
  `extensions/permissions/logs` and `extensions/agent` are all left out; symlinks and files over
  32 MiB are skipped and listed. `/backup list` shows the archives newest first and
  `/backup restore [file|folder] [--force]` verifies every entry (safe path, present in the archive,
  matching size and sha256) before writing anything — a single mismatch writes nothing. Existing
  files are refused unless `--force`, which first renames them to `<name>.bak-<stamp>`. Path
  arguments may start with `@` (the TUI completion prefix is dropped). The archive is written 0600
  and carries whatever credentials your extension configs hold. Off by default (Dev/Creator modes).
- **Dart and Nix code blocks are highlighted**: both languages were missing from the bundled
  syntax set and rendered as plain text. They now ship with the binary. Gradle has no grammar of its
  own, so `gradle` fences and `*.gradle` files are highlighted as Groovy.
- **Starting a session whose working directory is gone now asks first**: `--session`, `--session-id`,
  `--resume` and `--continue` used to open the session with a path that no longer exists, which made
  every tool call fail. It now offers `Continue` (use the current directory for this run, ask again
  next time), `Fork` (copy the session into the current directory as a new session and keep the
  original) or `Cancel`. Without a terminal on stdin the run fails with the paths spelled out.
- **GitHub Copilot's `Microsoft-Decision-1` works as a classifier**: Copilot serves the classifier at
  `/v1/systemone` on the account's own endpoint, so the request went to a path that does not exist.
  It now uses `/v1/systemone` and takes the host from the Copilot token, like the chat APIs do.
- **`/debug context` shows what is filling the context window**: the status bar reports one
  number — the tokens the provider last measured — and it says nothing before a turn has reported
  usage, when the system prompt and the tool schemas are already tens of thousands of tokens.
  `/debug context` splits the window by source instead: the system prompt (with `base`, project
  context, skills and MCP sections counted apart), the tool schemas actually declared for this
  request (name + description + parameter JSON; the hidden ones do not count), and the
  conversation. With a measured usage the known parts are counted and the difference goes to the
  conversation; before the first turn all three parts are estimates and the total is their sum, so
  it never reads 0. The rest of the window prints as `free`. Part of the `debug` extension, which
  stays off by default (Dev mode).
- **`/debug tools` prints the tool prompts**: the declarations actually sent for this request — name,
  description and parameter JSON, with hidden ones excluded — from the same source as the exported
  HTML's tool list. Part of the `debug` extension (off by default, Dev mode).
- **Desktop actions say whether they landed**: `computer_ui_action` now reads the target before
  acting and refuses with a reason when it is minimized, still settling, disabled, or does not
  advertise a matching action — the refusal says whether waiting can help. It then reads the target
  back and ends the reply with a `delivery:` line (`not-delivered` = retry safe,
  `delivered-unverified`, `delivered-verified`), so a failed write is reported as
  `[VALUE_NOT_APPLIED]` instead of looking like success. `click_text` uses a semantic click action
  by default and leaves the user's cursor alone; `headed: true` restores the old pixel click. When
  the platform cannot report a state, the action still runs and the doubt is noted rather than
  blocking it.
- **`computer_actions` can wait on a condition**: a `wait` step given a `ref` polls until a predicate
  holds (`exists`, `enabled`, `visible`, `actionable`, `value`) or the budget runs out, instead of
  sleeping a fixed number of seconds and hoping.
- **The `shrink` extension compacts tool results by content kind**: a busy repo's `git status`,
  a noisy build log or a 300-hit grep can fill the context window while carrying almost no
  information. With the extension on (off by default, Dev mode) every tool result above `minBytes`
  (2 KB) is classified as search hits, logs, diffs or JSON arrays and re-rendered by information
  density: hits grouped per file with `[+N more]`, logs keeping head, tail and error/warning lines,
  diffs keeping changed lines and collapsing unchanged runs, object arrays as a compact table.
  Nothing is lost — the full original is written to a temp file named in the trailing
  `[shrunk …]` line, and `keepFull = false` makes it discard instead. A result is never made
  longer and never replaced by an error; error results pass through untouched. `/shrink` prints
  the settings and the bytes saved in the session, `/shrink reset` clears them. Configure it in
  `extensions/shrink.json`.
- **`/skills check` reports what will break a skill at run time**: a skill whose `scripts/run.sh`
  has no exec bit, no shebang, or a shebang naming an interpreter this machine does not have fails
  as a bare `Permission denied` / `command not found`, and the agent then guesses. `/skills check`
  walks every loaded skill (disabled ones included) and reports those three cases per script with
  its path inside the skill. It only reports — nothing is changed and no skill is blocked. Bundled
  skills also now land executable when installed, so this check has nothing to say about them.
  The GUI's `/skills` takes the same subcommands: `check` runs the preflight, `settings` opens the
  skills panel.
- **Credentials can be configured without starting a session**: `prux auth login <provider>` runs
  the same account login as `/login`, only in the terminal: the URL (and device code) is printed,
  a local browser redirect is picked up automatically, and pasting the redirect URL or `code#state`
  also works. `--api-key` prompts for a key instead and `--no-browser` only prints the URL.
  `prux auth logout <provider>` removes the stored credential. Both exit non-zero with a message when
  the provider is unknown or nothing is stored.
- **A session whose directory is gone asks what to do instead of failing**: reopening such a session
  (`/import`, `/resume`, the sidebar and the session pickers in both interfaces) now offers
  `Continue` (run it in the current directory this once, ask again next time), `Fork` (copy it into
  the current directory as a new session, original kept) or `Cancel`.

- **Inbound review of an existing PR or issue**: `/review <URL> [--comment]` (the `review` extension,
  off by default) fetches the PR or issue with `gh`, checks out `refs/pull/<n>/head` into a read-only
  copy under `agent_dir()` with `git init` + `git fetch --depth 1` — the user's checkout, remote refs
  and branches stay untouched — saves GitHub's own three-dot diff next to it, and hands the agent a
  brief: absolute checkout path, diff, changed files and `file:line` reporting rules. With `--comment`
  the last answer of that turn is posted back as a comment once the turn settles: the body starts with
  a `<!-- prux-review:v1:<hash> -->` marker (hash = target + session), existing comments are scanned
  for it first (a hit reuses that link and posts nothing), and a post that a re-read cannot find is
  reported as `unconfirmed` and never retried. `/review status` shows the target, its checkout and the
  published comments; `/review clean` deletes every copy.

### Changed

- **Claude models that manage effort per message now say so**: for models whose catalog entry
  declares `supportsMidConvoEffort`, every request is sent with adaptive thinking, a fixed top-level
  effort of `low`, and an empty system message carrying the real level before each assistant turn —
  so a level switched mid-conversation (or capped by an organization) no longer makes the whole
  request fail. Temperature is not sent for these models.
- **`compat.requiresToolsWithToolHistory` keeps old gateways working**: when a `models.json` entry
  sets it, a request that declares no tools but whose history contains tool calls now sends
  `tools: []` instead of omitting the field.
- **Extension overlays report themselves as blocked**: while `ctx.ui.custom()` shows a full-screen
  overlay, the terminal status line (OSC 7501) now reports `blocked` with the overlay's title.
- **New sessions default to each provider's current flagship model**: the built-in per-provider table
  now points at the models the catalog lists as current — `anthropic` → `claude-opus-5-5`, `openai` /
  `azure` / `github-copilot` → `gpt-6.1-sol`, `google` → `gemini-4-argon`, `moonshotai` /
  `moonshotai-cn` / `opencode` → `kimi-k2.7-code`, `opencode-go` → `kimi-k3.1-pro`, `minimax` /
  `minimax-cn` → `MiniMax-M3`, `baseten` → `zai-org/GLM-5.3`, `vercel-ai-gateway` →
  `zai/glm-5.3`, `huggingface` → `moonshotai/Kimi-K2.7-Code`, `qwen-token-plan` /
  `qwen-token-plan-cn` → `qwen3.8-max`. Only the fallback used when nothing is chosen changes; a
  default saved in `settings.json` or a `--model` argument still wins.
- **`qwen-token-plan-individual-cn` is a provider of its own**: the Token Plan Individual
  allowlist served from the China endpoint, authenticated with `QWEN_TOKEN_PLAN_CN_API_KEY`.
- **`tool_search` ranks tools by name first**: a query term that hits a tool's name now counts three
  times as much as the same term inside its description, and a query that spells out the whole name
  (`thread read` → `thread_read`) is ranked first outright. Near-identical names such as `thread_read`
  and `queue_read` no longer swap places because one description happens to repeat the word more.

- **Memory recall also fires at the tool boundary**: the `memory` extension used to recall only from
  the user's own sentence, which never names the file being edited. Every tool call now contributes a
  second recall keyed on what that call touches — paths, file names with extensions, snake_case and
  camelCase identifiers from the arguments (plain lowercase words are ignored) — and the hit cards are
  appended after the tool result under a `[memory:tool]` header, leaving the result text byte for byte
  unchanged. Each `(entry, updated_at)` is delivered once per session, at most `toolRecallLimit`
  (default 3) cards per call, capped by the injection budget and 1500 characters; `off` in the
  settings panel disables the second path.
- **The `/ps` panel shows CPU and memory per background job**: each running row now carries the CPU
  percentage and resident memory of the job's whole process tree — a `sh -c cargo build` job does its
  work in descendants, so the leader alone would read as idle. Sampling runs every two seconds, the
  first sample reports `0.0%` (a percentage needs two refreshes to difference) and an unreadable tree
  shows `--` instead of a fake zero.

### Fixed

- **Stopping a background job can no longer kill an unrelated process group**: a job's pid is kept in
  a table and killed later, but a command that had already exited and been reaped may have had its
  pid reused — `killpg` would then hit whoever owns it now. Stopping (and `abort_bash`) now re-reads
  the pid's identity (machine boot time plus process start time) and sends nothing when it no longer
  matches, reporting `had already exited` instead.
- **Long streaming replies re-rendered every block each frame**: a message was laid out from its
  first character on every frame while it streamed. Markdown is now laid out block by block and each
  block is reused until its text, width, base style or theme changes, so only the tail of a streaming
  reply is re-rendered.
- **Streaming tool arguments no longer re-parse the whole buffer**: every delta used to re-parse the
  accumulated JSON from the start (plus a full copy to repair it), so a large `write` grew quadratically
  — a 90 KB call peaked around 140 MB of heap and showed stale arguments once past the old 64 KB cut-off.
  Arguments are now parsed incrementally, one pass over each delta, and the shown snapshot refreshes at
  most every 4 KB.
- **Deeply nested quotes multiplied their lines**: each level stripped two columns and re-wrapped the
  inner lines, and below a certain width the wrapping stopped shrinking and kept multiplying — a
  30-level quote could stall rendering. Nesting past 8 levels is now rendered flat, and the content
  stays visible.
- **Pasting inside WSL did nothing**: without WSLg there is no X11/Wayland clipboard, so Ctrl+V found
  nothing. It now falls back to the Windows clipboard through `powershell.exe Get-Clipboard`.
- **Exiting could lose a freshly rotated OAuth token**: a refresh runs in its own task and writes the
  new token when the provider answers; quitting while one was in flight discarded it and forced another
  `/login`. The TUI, the desktop app and ACP now wait (up to 5 s) for in-flight refreshes to land.
- **A `ref` on Linux was not re-checked before acting**: the macOS and Windows backends re-read the
  element and refuse when its name or role changed, but the AT-SPI path acted on the recorded
  `(bus, path)` directly, so a reused path could drive a different control. Every Linux action and
  read now verifies the identity first and fails with `[STALE_REF]`.
- **Chinese in monospace text fell back to a system font**: code blocks, inline code and the diff
  view asked for Source Code Pro only, which carries no CJK glyphs, so each Chinese character was
  drawn by whichever font the system picked — different on every machine and visibly unlike the
  rest of the UI. Monospace text now declares the embedded CJK font as its glyph fallback.
- **Claude tools that share schema definitions lost them**: the request kept only `properties` and
  `required` and dropped the schema's root `$defs`/`definitions`, so every property `$ref` pointed at
  nothing — tools whose parameters share a nested model (common for MCP servers) made the model send
  the wrong arguments, and some compatible gateways rejected the request outright. Both are sent now.
- **Gemini rejected tools whose schema uses `const`**: Gemini ignores `const` in
  `parametersJsonSchema` and honors `enum` only, so such a tool failed the whole request. String and
  number `const` values are now sent as single-value `enum`s; literal data in `default`, `examples`
  and existing `enum`s is left untouched.
- **OpenAI Responses sent the affinity header under the wrong name**: it used `session_id` instead of
  the documented `session-id`, so the server-side session affinity never took effect. Responses now
  sends `session-id`; OpenAI-compatible completions keep `session_id`.
- **Some transient failures were not retried, and a plan limit was**: a stream a gateway cut off after
  the reply began, a socket the other side closed, and a ChatGPT subscription whose usage data is
  briefly unavailable now retry. Zhipu/Z.ai plan-limit errors (code `1308`, sent as HTTP 429) no
  longer retry until the cap resets, and neither do OpenCode Go's "available balance" limits.
- **LiteLLM's context overflow was not recognised**: its `ContextWindowExceededError` wording
  (`context window exceeded`) was missing from the overflow patterns, so a request that only needed
  compaction failed instead of being compacted and retried.
- **Gateways that reject `content: null` now get an empty string**: an assistant message that carries
  only tool calls used to send `content: null` unconditionally. Set
  `compat.requiresNonNullAssistantContent` on such a model in `models.json` and it sends `""`.
- **A saved "do not trust" was ignored for projects that need no trust**: when a project had nothing
  that requires trust (for example only an `AGENTS.md`), startup short-circuited to trusted and never
  looked at the saved decision, so the project's MCP servers, subagent configs and memory were read
  as trusted. A saved decision now wins; auto-trust applies only when no decision was saved.
- **Prompt template arguments broke on apostrophes**: an apostrophe inside a word (`doesn't`) opened a
  quote and swallowed the rest of the line, and `\"` stayed a backslash and a quote. Quotes now start
  only at the beginning of an argument, an unclosed quote stays text, and `\"` / `\'` give a literal
  quote.
- **Search ranked a scattered hit above a better contiguous one**: `/model`, `/resume` and the file
  suggester scored only the first possible alignment, so `opus` could list a loose match before
  `claude-opus-4.1`. Every alignment is scored now and the best one wins.
- **`tool_search` could not find anything with a Chinese query**: the tokenizer kept ASCII letters and
  digits only, so a query like `搜索` became empty and every non-English search returned nothing.
  Letters of any script now match, with CJK text split per character.
- **`grep` read its `glob` from the wrong folder**: with `path: "repo"`, a glob such as
  `migrations/main/*.js` matched nothing because it was resolved against the directory the agent was
  started in. Globs are now relative to `path`, and a leading `./` is ignored.
- **`grep` with `context` read whole files into memory**: context lines came from reading each matched
  file in full, which ran out of memory on large files and shifted line numbers in files with lone
  carriage returns. They now come from ripgrep itself.
- **`grep` and `find` reported the limit when the result count was exactly the limit**: the notice said
  `Use limit=… for more` although nothing was cut. It appears only when there really are more results.
- **`find` ignored Windows-style path patterns**: `src\**\*.ts` was not treated as a path pattern at
  all, so it matched nothing. Separators are normalised, and a leading `./` is ignored.
- **`read` loaded the whole file even with `offset`/`limit`**: opening a huge log ran out of memory.
  Only the file head is read now (to recognise images) and the requested line range is streamed, so
  `offset` near the end of a multi-gigabyte file costs nothing.
- **`bash` could wait forever when a background process held the pipe**: the timeout covered only the
  shell's exit, so `cmd & exit 0` with the background job still writing hung the tool. The timeout now
  covers the output drain too, and the error reports the shell's real exit code and when it exited.
- **Exported HTML did not show the system prompt and tools**: `/export`, sharing and `--export` wrote the
  entries only, so the two sections that explain what the session ran with stayed empty. All of them now
  carry the system prompt and the tool declarations actually sent for this request (hidden ones excluded);
  `--export` reads the snapshot the session records while running.
- **HTML comments in Markdown showed up as text**: `<!-- ... -->` was rendered as visible text in the
  TUI and the GUI. Comments are dropped; other inline HTML is unchanged.
- **Orca fell back to text for images and lost true color**: Orca is now recognised as a true-color
  terminal, and uses Kitty images when it advertises `ORCA_IMAGE_PROTOCOL=kitty`.
- **A malicious MCP server could hand the browser a non-web URL**: OAuth metadata and the final
  authorization URL were accepted with any scheme, so a server could supply `file:` or a custom
  protocol such as `search-ms:` that the platform URL handler launches locally. Only `http:` and
  `https:` are accepted now.
- **A large streamed tool call re-parsed its whole argument buffer on every delta**: each chunk
  re-scanned the accumulated JSON from the start, so a big `write` cost minutes of CPU and hundreds
  of MB of heap. Past 64 KB the intermediate deltas are no longer re-parsed; the final arguments are
  still parsed in full once the call is complete.
- **A failed session write left an entry that never reached the disk**: the entry went into memory
  anyway, so later entries chained to a parent the file does not have and a resumed session lost
  that history and its tool results. An entry is now applied to memory only after it is written;
  the failure is still reported.
- **`--model` could not name a provider whose id contains a slash**: the split took the first slash,
  so a URL-derived provider id such as `llama/local` lost its second segment. The provider prefix
  is now matched against the longest known provider id.
- **Deeply nested blockquotes could render without bound**: every level re-wrapped its inner lines,
  so the line count multiplied until layout stalled. A message that would render past 20,000 lines is
  cut there with a notice instead.
- **PageUp/PageDown did nothing in settings sub-panels**: the checked-list sub-panel only moved one
  row per key. Both paging keys now move ten rows there too.
- **ACP could cut its own cleanup short**: stdin EOF and SIGTERM arriving together dropped the
  in-flight work, so a refreshed MCP token or a session write could be lost. The first EOF or signal
  now lets the current turn finish, and only a second one exits immediately.
- **The GUI showed a freshly read image only after switching sessions**: the live
  `tool_execution_end` event carried the result text alone, so an image that had just been read
  stayed invisible until the transcript was rebuilt from history. The event now carries the
  attachments as image blocks, in the same shape and order as the `toolResult` message.
- **F1–F12 typed invisible characters into the editor**: under the kitty keyboard protocol
  (kitty, ghostty, foot, wezterm) these keys arrive as private-use codepoints, and only F13+ and the
  lock keys were translated — F1–F12 fell through to a plain character, so a keypress left an
  invisible glyph in the input box. F1–F12 are now turned back into the real function key, and any
  other private-use codepoint is dropped instead of being inserted.
- **A resized photo was sent as PNG even when JPEG was several times smaller**: the encoder took the
  first candidate under the byte limit and PNG always came first, so a large photo paid for a
  lossless encoding nobody asked for. PNG and the JPEG at the configured quality are now compared
  and the smaller one wins; lower JPEG quality is used only when neither fits.
- **A display formula wider than the terminal was broken up character by character**: a fraction or
  a sum is a 2D layout and cannot be wrapped row by row, so splitting it destroyed the meaning. A
  formula whose rendered output does not fit the available width now falls back to its LaTeX source.
- **Editing around a collapsed paste could leave the marker half-deleted**: only Backspace treated
  `[paste #N …]` as one unit, so Delete, word deletes, kill-to-line-end and typing inside the marker
  could split it — the marker stopped matching, its stored text was dropped, and the leftover
  fragment was sent as the prompt. Paste markers are now atomic for every edit.
- **A directory name could inject escape sequences into the terminal title**: the window title is
  built from the current directory's name and written as an OSC sequence, with nothing stripped, so a
  folder named with an embedded escape byte could reach the terminal. Control characters are removed
  before the title is written.
- **The reported cost ignored what the provider billed**: with OpenRouter the response carries the
  actual amount, but the total was always recomputed from catalog prices, so the number differed from
  the invoice. A reported `usage.cost` is now kept as the total (the per-item lines still come from
  the catalog rates) and `/session` marks it as `provider-reported`. A reported `0` is not trusted:
  subscription endpoints always send `cost: 0`, and taking it would wipe an estimated total.
- **Confirming a device-code login cancelled it**: on the login panel an empty input plus Enter meant
  "cancel", so the natural way to say "yes, open the page" threw the login away. Enter now opens the
  verification URL and copies the code to the clipboard (either step failing only prints a hint);
  cancelling stays on Esc. The desktop interface offers the same by click: the code copies itself and
  the URL opens the browser.
- **Display formulas lost their alignment**: the TUI dropped the leading spaces on every line when
  wrapping, so a stacked formula's numerators, denominators and sum limits collapsed against the left
  edge while the fraction rule stayed where it was. Leading spaces that come from the source are kept
  now; only the spaces a wrap introduces are dropped.
- **A batch of small math-rendering defects**: `\partial t` rendered as `∂ t` because of the space in
  the source, a leading minus merged into the fraction rule, every stacked fraction carried a trailing
  space (`── ,`), `90^\circ` came out as `90^∘`, and a symbol with only a subscript took two lines.
  Spaces after a control word are now eaten as TeX does, a minus is U+2212, a stacked fraction no
  longer pads its right side, `\circ` as a superscript gives `°`, and a script on one side only stays
  inline (`x_c`, `x^n`) while a sub- and superscript pair still stacks. One unknown command throws
  away the whole formula, so `\ulcorner` / `\urcorner` alone kept `G \leftrightarrow
  \neg\mathbb{Provable}(\ulcorner G\urcorner)` from rendering at all; the corner brackets, the
  card suits, `\lnot`, `\surd`, `\boxplus`/`\boxtimes`, `\lesssim`/`\gtrsim`, `\nleq`/`\ngeq`
  and a handful of other common symbols are in the table now. `\mathbb` also stops half-converting a
  word it has no glyphs for (`\mathbb{Provable}` gave `ℙrovable`, it stays `Provable`).
- **Windows path containment was case-sensitive**: `C:\Repo` and `C:\repo\file` were treated as
  unrelated, so a file inside the project could be reported as outside it. The comparison now folds
  case on Windows only; component-wise matching on other platforms is unchanged.

## 中文

### 新增

- **`/backup` 快照与恢复 prux 自身资产**：新增的 `backup` 扩展把 `agent_dir()` 下的 `settings.json`、
  `APPEND_SYSTEM.md`、`trust.json`、`themes/`、`skills/` 与 `extensions/`（扩展配置、记忆、MCP 配置、
  workflows）打成一个 zip——默认落到 `agent_dir()/backups/prux-backup-<时间戳>.zip`，也可以指定目录。
  归档首个条目 `manifest.json` 记录 prux 版本、来源目录与每个文件的大小与 sha256。**只收 `agent_dir()`**：
  项目里的 `.prux/` 与 `AGENTS.md` 跟着仓库走，不进归档。凭据不进归档：`auth.json`、`sessions/`、
  `history/`、`logs/`、`bin/`、`docs/`、可重建的 `models-store.json` 与 `recent-cwds.json`、归档目录自身，
  以及 `extensions/worktrees`、`extensions/permissions/logs`、`extensions/agent` 全部排除，符号链接与
  超过 32 MiB 的单文件跳过并列出。`/backup list` 按时间倒序列出归档；`/backup restore [文件|目录] [--force]`
  先逐条校验（路径合法、条目齐全、大小与 sha256 一致）再写盘——有一条对不上就一个字节都不写。
  目标已存在且不给 `--force` 时拒绝并点名冲突文件，`--force` 覆盖前先把原文件改名成
  `<名字>.bak-<时间戳>`。路径参数可以 `@` 开头（TUI 补全的前缀会被去掉）。归档按 0600 写盘，
  里面会带扩展配置持有的凭据。默认不启用（`Dev`/`Creator` 模式）。
- **Dart 与 Nix 代码块能高亮了**：这两种语言不在内置语法集里，此前只能当纯文本显示，现在随二进制一起内嵌。
  Gradle 没有独立语法，`gradle` 围栏与 `*.gradle` 文件按 Groovy 高亮。
- **恢复的会话工作目录已消失时会先问怎么办**：`--session`、`--session-id`、`--resume` 与 `-c` 以前直接拿一个
  不存在的路径开会话，之后每个工具调用都失败。现在给三选——`Continue`（本次用当前目录，下次仍会问）、
  `Fork`（复制成当前目录下的新会话，原会话保留）、`Cancel`。stdin 不是终端时直接报错并列出两个路径。
- **GitHub Copilot 的 `Microsoft-Decision-1` 可以当分类器用了**：Copilot 把分类器挂在账号端点下的
  `/v1/systemone`，此前请求打到了不存在的路径。现在用 `/v1/systemone`，域名与聊天接口一样取自 Copilot token。
- **`/debug context` 说明上下文窗口被什么占着**：状态栏只给一个数——供应商上次实测的 token——
  而在第一轮落地 usage 之前它什么都不说，可那时系统提示词与工具 schema 已经占了几万 token。
  `/debug context` 改成按来源拆开：系统提示词（`base`、项目上下文、技能、MCP 段分别计数）、
  本次请求真正声明的工具 schema（名字 + 描述 + 参数 JSON，被隐藏的不算）、以及会话本身。
  有实测 usage 时已知部分按词表数出来、差额归会话；第一轮之前三块都是估算、总额取三块之和，
  因此不会再显示 0。窗口剩下的部分打成 `free`。属于 `debug` 扩展，仍然默认不启用（Dev 模式）。
- **`/debug tools` 打印工具提示词**：本次请求真正声明的工具——名字、描述、参数 JSON，被隐藏不声明的
  不算——与导出 HTML 的工具有同一份来源。属于 `debug` 扩展（默认不启用，Dev 模式）。
- **桌面动作会告诉你到底生效没生效**：`computer_ui_action` 执行前先读一次目标，被最小化、还在动、
  已禁用或没声明对应能力就带着原因拒绝，并说清「等一等有没有用」；执行后再读一次，回执最后一行是
  `delivery:` 判定（`not-delivered` = 重试安全、`delivered-unverified`、`delivered-verified`），
  写值没进去会报 `[VALUE_NOT_APPLIED]` 而不是看起来成功。`click_text` 默认走语义点击、不动你的光标，
  `headed: true` 才回到原来的像素点击。平台读不到的状态不阻断动作，只把不确定写进回执。
- **`computer_actions` 的 `wait` 步可以等条件成立**：给了 `ref` 就轮询到谓词成立（`exists`、
  `enabled`、`visible`、`actionable`、`value`）或用尽预算，而不是固定睡几秒碰运气。
- **`shrink` 扩展按内容类型压缩工具结果**：忙碌仓库里的一条 `git status`、一屏构建日志、几百条
  grep 命中，几乎不携带信息却能把上下文窗口吃掉。开启该扩展（默认关闭，Dev 模式）后，超过
  `minBytes`（2 KB）的工具结果会被识别成搜索命中 / 日志 / diff / JSON 数组并按信息密度重排：
  命中按文件分组、每组留够数后标 `[+N more]`，日志留首尾与错误警告行，diff 只留变更行并把
  连续未变更折成一行，对象数组渲染成紧凑表格。什么都不丢——完整原文写进临时文件，路径就在末尾
  那行 `[shrunk …]` 里，`keepFull = false` 时才直接丢弃。结果永远不会被压得更长，也不会被
  换成错误，失败结果原样通过。`/shrink` 打印当前设置与本会话省下的字节，`/shrink reset` 归零。
  配置见 `extensions/shrink.json`。
- **`/skills check` 说出技能跑起来会怎么坏**：`scripts/run.sh` 没有执行位、没有 shebang、或
  shebang 指的解释器本机没装，症状都是一句干巴巴的 `Permission denied` / `command not found`，
  之后模型就开始猜。`/skills check` 走一遍已加载的全部技能（含被禁用的），逐个脚本报这三类问题
  并带上它在技能内的路径。它只报告——不改任何东西，也不拦任何技能。内置技能在安装时也会带上
  执行位，所以它对这些技能没什么可说的。桌面 GUI 的 `/skills` 收同样的子命令：`check` 跑预检，
  `settings` 打开技能面板。
- **不起会话也能配置凭据**：`prux auth login <provider>` 跑与 `/login` 同一套账号登录，只是全在终端里：
  打印授权地址（设备码流程还打印验证码），本机浏览器重定向会被自动接住，粘回回调 URL 或
  `code#state` 同样可用；`--api-key` 改为提示粘贴 key，`--no-browser` 只打印地址不开浏览器。
  `prux auth logout <provider>` 删除已存凭据。provider 未知或没有可删的凭据时以非零退出码报错。
- **存储目录已消失的会话会先问怎么办**：重新打开这类会话（`/import`、`/resume`、两个界面的侧栏与
  会话选择器）现在给三选——`Continue`（本次用当前目录，下次仍会问）、`Fork`（复制成当前目录下的
  新会话，原会话保留）、`Cancel`。

- **把别人的 PR / issue 拉进来评审**：`/review <URL> [--comment]`（`review` 扩展，默认关闭）用 `gh` 取
  元数据与 GitHub 自己的三点 diff，在 `agent_dir()` 下用 `git init` + `git fetch --depth 1` 把
  `refs/pull/<n>/head` 检出一份只读副本（不动用户仓库、不建 worktree、不写远端引用、不切分支），
  再把评审简报交给 agent：副本绝对路径、diff、改动清单与 `file:line` 报点口径。带 `--comment` 时
  这一轮的最后一条回答会在结算时作为评论发回去——正文首行是 `<!-- prux-review:v1:<hash> -->`
  marker（哈希 = 目标 + 会话 id），投递前先按 marker 扫已有评论（命中就复用那条链接、不重复发），
  发完复核、扫不到就报 `unconfirmed` 且不再重试。`/review status` 看目标 / 副本 / 已发布评论，
  `/review clean` 删掉全部副本。

### 变更

- **按消息管理 effort 的 Claude 模型现在照实下发**：目录条目声明了 `supportsMidConvoEffort` 的模型，
  请求改为 adaptive thinking + 固定顶层 `low`，并在每条 assistant 消息前用一条空 system 消息携带当时的真实档位——
  会话中途换档、或被组织级限额压住时不会整条请求失败。这类模型不再发送 temperature。
- **`compat.requiresToolsWithToolHistory` 保住老网关**：`models.json` 条目设了它时，没有工具可声明、
  但历史里有工具调用的请求会发 `tools: []` 而不是省略该字段。
- **扩展覆盖层会把自己报成阻塞**：`ctx.ui.custom()` 的全屏覆盖层显示期间，终端状态行（OSC 7501）
  现在上报 `blocked` 并带上覆盖层标题。
- **新会话默认用各 provider 当前的主力模型**：内置默认模型表改指目录里标为当前的那批——`anthropic` →
  `claude-opus-5-5`，`openai` / `azure` / `github-copilot` → `gpt-6.1-sol`，`google` →
  `gemini-4-argon`，`moonshotai` / `moonshotai-cn` / `opencode` → `kimi-k2.7-code`，`opencode-go`
  → `kimi-k3.1-pro`，`minimax` / `minimax-cn` → `MiniMax-M3`，`baseten` → `zai-org/GLM-5.3`，
  `vercel-ai-gateway` → `zai/glm-5.3`，`huggingface` → `moonshotai/Kimi-K2.7-Code`，
  `qwen-token-plan` / `qwen-token-plan-cn` → `qwen3.8-max`。改的只是「什么都没选」时的回退；
  `settings.json` 里存过的默认值或 `--model` 参数仍然优先。
- **新增 `qwen-token-plan-individual-cn` provider**：中国端点上的 Token Plan Individual 白名单，
  凭据为 `QWEN_TOKEN_PLAN_CN_API_KEY`。
- **`tool_search` 先按工具名排序**：查询词命中工具名的权重是命中描述时的三倍，查询正好拼出整个名字
  （`thread read` → `thread_read`）时直接置顶。`thread_read` 与 `queue_read` 这类近似名不再因为其中一个
  描述里多重复几次关键词就换位。

- **记忆召回也发生在工具边界**：`memory` 扩展原先只在用户那句话上召回，而那句话里从来不会出现正在改的
  文件名。现在每次工具调用都会再召回一次，查询取自这次调用碰到的路径、带扩展名的文件名、蛇形 / 驼峰标识符
  （普通小写单词不算），命中条目带 `[memory:tool]` 标题**追加在工具结果后面**，结果正文逐字不动。
  同一 `(条目, 更新时间)` 每个会话只投一次，单次至多 `toolRecallLimit`（缺省 3）条、且不超过注入预算与
  1500 字符；设置面板里选 `off` 可关掉。
- **`/ps` 面板显示每个后台 job 的 CPU 与内存占用**：运行中的每一行带上整棵进程树的 CPU 百分比与
  常驻内存（`sh -c cargo build` 的活是后代在干，只报组长会显示成空转）。每两秒采样一次，首次采样
  CPU 报 `0.0%`（百分比要靠两次刷新的差值），采不到时显示 `--`，不假装 0。

### 修复

- **停后台 job 不会再误杀无关的进程组**：job 的 pid 是记在表里的旧值、过一阵才用，而命令可能早已退出
  并被回收、同一个 pid 已经被复用——那时 `killpg` 杀掉的是别人的进程组。停止（含 `abort_bash`）现在会
  重新读一次 pid 的身份（机器启动时刻 + 进程启动时刻），对不上就一个信号都不发，改报「already exited」。
- **长回复流式期间每帧重排所有块**：消息每帧都从第一个字符重新排版。现在 markdown 按顶层块排版并逐块复用，
  只有文本、宽度、基础样式或主题变化的那块才重新排版，流式回复只重排末尾一块。
- **流式工具参数不再整串重解析**：每个 delta 都从头的重新解析累积 JSON（必要时还要整串拷贝做修复），
  大 `write` 调用因此是平方级增长——90 KB 的参数峰值吃掉约 140 MB 堆，越过旧的 64 KB 阈值后参数干脆不再更新。
  现在参数走增量解析，每段只扫一遍新增字符，展示用快照最多每 4 KB 刷新一次。
- **深层嵌套引用让行数乘法增长**：每层剥掉两列宽再把内层行重新折一次，宽度到一定程度后不再收缩、
  于是继续成倍增长——30 层引用能把渲染卡住。超过 8 层后不再缩进加边框，内容照常显示。
- **WSL 里粘贴没反应**：没有 WSLg 时既无 X11 也无 Wayland 剪贴板，Ctrl+V 取不到任何内容。
  现在回退到经 `powershell.exe Get-Clipboard` 读 Windows 侧剪贴板。
- **退出时可能丢掉刚轮转的 OAuth token**：刷新跑在独立任务里，provider 答完才写盘；此时退出会把它丢弃，
  下次只能重新 `/login`。现在终端界面、桌面应用与 ACP 退出前都会等在途刷新落盘（最多 5 秒）。
- **Linux 上按 `ref` 操作前不复核节点身份**：macOS 与 Windows 后端会重读元素、名称或角色变了就拒绝，
  但 AT-SPI 那条路径直接拿记录的 `(bus, path)` 动手，路径被复用时会操作到别的控件。
  现在每个 Linux 动作与读值都先复核身份，对不上就报 `[STALE_REF]`。
- **等宽文本里的中文落到系统字体**：代码块、内联代码与 diff 视图只指定 Source Code Pro，它没有中文字形，
  于是每个汉字都由系统随手挑的字体画出来——各机器不一样，也和界面其余部分明显不搭。
  现在等宽文本声明内嵌中文字体作字形回退。
- **Claude 上共享 schema 定义的工具丢了定义**：请求只保留 `properties` 与 `required`，把 schema 根部的
  `$defs`/`definitions` 一起丢了，属性里的 `$ref` 于是指向空——多个工具共用嵌套模型时（MCP 服务器常见）
  模型会给出错误参数，部分兼容网关直接拒绝请求。现在两者一并下发。
- **Gemini 拒绝带 `const` 的工具 schema**：Gemini 忽略 `parametersJsonSchema` 里的 `const`、只认 `enum`，
  于是整个请求失败。现在字符串与数字的 `const` 改写成单值 `enum`；`default`、`examples` 与已有 `enum`
  里的字面数据原样保留。
- **OpenAI Responses 的会话亲和头用错了名字**：发的是 `session_id`，而文档里是 `session-id`，
  服务端会话亲和一直没生效。Responses 现在发 `session-id`；OpenAI 兼容的 completions 仍是 `session_id`。
- **有些瞬时故障没重试，套餐上限却在重试**：网关在回复已开始后截断的流、对端关闭的 socket、
  ChatGPT 订阅用量数据短暂不可用，现在都会重试。Zhipu/Z.ai 的套餐上限（错误码 `1308`，以 HTTP 429 下发）
  在额度重置前不再重试，OpenCode Go 的「可用余额」限额同样不重试。
- **LiteLLM 的上下文溢出没被识别**：溢出文案表里缺 `context window exceeded`（`ContextWindowExceededError`），
  本该压缩后重试的请求直接失败了。
- **拒绝 `content: null` 的网关现在收到空串**：只带工具调用的 assistant 消息原先无条件发 `content: null`。
  在 `models.json` 里给这类模型设 `compat.requiresNonNullAssistantContent` 即改发 `""`。
- **已保存的「不信任」对无需信任的项目不生效**：项目里没有需要信任的资源时（比如只有一份
  `AGENTS.md`），启动直接短路成「信任」、根本不看已保存的决策，于是项目的 MCP 服务器、子代理配置
  与记忆都按已信任读取。现在已保存的决策优先，没有决策时才自动信任。
- **提示词模板参数遇到撇号就串行**：词内撇号（`doesn't`）会开引号并吞掉后面整行，`\"` 也仍是
  反斜杠加引号。现在引号只在参数起始处生效，未闭合的引号按文本保留，`\"` / `\'` 给出字面引号。
- **搜索把散落的命中排在更好的连续命中之前**：`/model`、`/resume` 与文件补全只给第一个可能对齐位置
  打分，`opus` 可能把松散命中排到 `claude-opus-4.1` 前面。现在每个对齐位置都打分并取最优。
- **`tool_search` 用中文查不到任何工具**：分词只保留 ASCII 字母与数字，`搜索` 这类查询分词后为空，
  所有非英文检索都返回空。现在任意书写系统的字母都能命中，中日韩文本按单字切词。
- **`grep` 的 `glob` 从错误的目录起算**：`path: "repo"` 时 `migrations/main/*.js` 这类 glob 一个都
  匹配不到，因为它相对的是 agent 的启动目录。现在 glob 相对 `path`，前导 `./` 被忽略。
- **`grep` 带 `context` 时把整个文件读进内存**：上下文行是靠整文件读入命中文件得到的，大文件会吃光内存，
  含孤立回车符的文件还会让行号错位。现在上下文行由 ripgrep 自己产出。
- **`grep` 与 `find` 在结果数恰好等于 limit 时也提示 limit**：明明没有截断却提示 `Use limit=… for more`。
  现在只有确实还有更多结果时才提示。
- **`find` 不认 Windows 风格路径模式**：`src\**\*.ts` 没被当成路径模式，一个文件都匹配不到。
  现在分隔符会归一，前导 `./` 被忽略。
- **`read` 给了 `offset`/`limit` 也整文件读入**：打开几百 MB 的日志会吃光内存。现在只读文件头判断图片、
  按行区间流式读取，在多 GB 文件末尾 `offset` 也不花代价。
- **`bash` 在后台进程占着管道时会一直等**：超时只管「shell 退出」，`cmd & exit 0` 而后台作业还在写时
  工具会挂住。现在超时也覆盖输出收尾，错误里还会带上 shell 的真实退出码与退出时间。
- **导出的 HTML 看不到系统提示词与工具**：`/export`、分享与 `--export` 只写会话条目，解释这次会话
  用什么跑起来的两节一直是空的。现在三者都带上系统提示词与本次真正声明的工具（被隐藏的不算）；
  `--export` 读的是会话运行时记下的快照。
- **Markdown 里的 HTML 注释被当正文显示**：`<!-- ... -->` 在 TUI 与 GUI 里都会被画出来。现在注释被丢弃，
  其余内联 HTML 行为不变。
- **Orca 终端的图片退化成文本、也没有 truecolor**：现在 Orca 被识别为支持 truecolor，
  并在宣告 `ORCA_IMAGE_PROTOCOL=kitty` 时使用 Kitty 图片。
- **恶意 MCP 服务器可以把非 web 地址交给浏览器**：OAuth 元数据与最终授权地址原先接受任意 scheme，
  服务器给 `file:` 或 `search-ms:` 这类自定义协议时会被平台 URL handler 本地拉起。
  现在只接受 `http:` 与 `https:`。
- **大工具调用的流式参数在每个增量上重解析整串**：每来一段就把累积的 JSON 从头扫一遍，一次大
  `write` 能吃掉几分钟 CPU 和几百 MB 堆。现在超过 64 KB 后中间增量不再重解析，收尾时仍完整解析一次。
- **会话写盘失败时条目只留在内存里**：写入失败也照样应用，后续条目就挂到磁盘上没有的父节点上，
  恢复会话时那段历史与工具结果一起丢。现在只有写成功才应用进内存，失败仍然照报。
- **`--model` 指不了 id 含 `/` 的 provider**：原先按第一个斜杠切，`llama/local` 这类从 URL 派生的
  provider 会丢掉第二段。现在按最长已知 provider id 匹配前缀。
- **深层嵌套引用可以无限生成行**：每层都把内层行再折一次，行数乘法增长直到排版卡住。
  一条消息渲染超过 20000 行时在那里截断并附一行提示。
- **设置子面板里 PageUp/PageDown 不起作用**：勾选列表原先每次只移一行。现在两个翻页键都移十行。
- **ACP 会把自己的收尾切断**：stdin EOF 与 SIGTERM 同时到达时在途工作被直接丢弃，刷新到的 MCP token
  与会话落盘可能一起丢。现在首个 EOF/信号让当前回合跑完，第二个信号才立即退出。
- **GUI 里刚 read 进来的图片要切会话才出现**：实时 `tool_execution_end` 事件只带结果文本，
  刚读到的图片要等对话流按历史重建才看得见。现在事件按 `toolResult` 消息同形同序带上图片块。
- **F1–F12 会往编辑器里灌不可见字符**：kitty 键盘协议下（kitty、ghostty、foot、wezterm）
  这些键以私用区码点上报，而只有 F13+ 与几个 lock 键被翻译，F1–F12 落到普通字符兜底，
  按一下就在输入框留下看不见的字符。现在 F1–F12 还原成真正的功能键，其余私用区码点直接丢弃。
- **缩放后的照片明明 JPEG 小几倍却仍然发 PNG**：编码器取第一个满足字节上限的候选，而 PNG 永远排第一，
  于是大照片白白走了没人要求的无损编码。现在先比较 PNG 与「配置质量的 JPEG」取较小者，
  两者都放不下才逐档降低 JPEG 质量。
- **超宽的 display 公式被逐字符拆开**：分式、求和是 2D 布局，无法逐行折行，拆开就没了含义。
  渲染结果放不进可用宽度的公式现在回退到 LaTeX 原文。
- **在折叠粘贴附近编辑会把 marker 删成半截**：原先只有 Backspace 把 `[paste #N …]` 当一个整体，
  Delete、删词、删到行尾、以及在 marker 中间打字都会把它切开；切开后正则失配、存储被丢，
  残骸会被当成正文发出去。现在所有编辑操作都把 paste marker 当原子单元。
- **目录名能往终端标题里注入转义序列**：窗口标题由当前目录名拼成、以 OSC 序列写出，没有任何过滤，
  名字里夹 ESC 就能把控制序列送进终端。现在写标题前先剥掉控制字符。
- **计费合计没算供应商上报的金额**：用 OpenRouter 时响应里带着真实金额，但合计一律按目录单价重算，
  于是数字和账单对不上。现在上报的 `usage.cost` 直接作合计（分项仍按目录单价算），
  `/session` 会标成 `provider-reported`。上报 `0` 不采信：订阅型端点恒发 `cost: 0`，
  采信它会把按目录单价估出的合计清零。
- **确认设备码登录反而把登录取消了**：登录面板上空输入 + Enter 原本等于「取消」，
  于是想「好，打开页面吧」的那一下把登录丢了。现在 Enter 打开验证地址并把验证码复制到剪贴板
  （两步失败都只提示），取消仍是 Esc。桌面版用点击达成同一件事：点验证码即复制，点地址即开浏览器。
- **块级公式的分子分母塌到最左边**：终端界面折行时把每行的行首空格当成断行残留丢掉，靠空格对齐的分子、
  分母与求和上下限因此全部贴着左边，和分数线完全对不上。现在源码里显式给出的行首空格保留，
  只有自动换行产生的才丢。
- **数学公式排版的一批小毛病**：`\partial t` 因源码里的空格渲染成 `∂ t`，负号和分数线连成一条长横线，
  每处分式右侧都缀一个空格（`── ,`），`90^\circ` 写成 `90^∘`，只带下标的符号独占两行。
  现在控制词后的空格按 TeX 规则吞掉，减号改用 U+2212，堆叠分式右侧不再留空隙，
  `\circ` 上标给 `°`，单侧上下标走行内（`x_c`、`x^n`），上下标同时存在时才垂直堆叠。
  另外，一个不认识的命令就会让整条公式退回原文，所以缺个 `\ulcorner` / `\urcorner` 就足以让
  `G \leftrightarrow \neg\mathbb{Provable}(\ulcorner G\urcorner)` 完全渲染不出来——现在补上了
  直角引号、四种花色、`\lnot`、`\surd`、`\boxplus`/`\boxtimes`、`\lesssim`/`\gtrsim`、
  `\nleq`/`\ngeq` 等常见符号；`\mathbb` 也不会把没有黑板字形的词改一半
  （`\mathbb{Provable}` 原先是 `ℙrovable`，现在是 `Provable`）。
- **Windows 上路径包含判断区分了大小写**：`C:\Repo` 与 `C:\repo\file` 被当成无关路径，
  项目内的文件可能被判成「目录之外」。现在只在 Windows 上折叠大小写，其它平台仍按组件比较、行为不变。

