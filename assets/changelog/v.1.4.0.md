## English

### Added

- **The `ask-user-question` extension lets the model ask instead of guess**: it adds one
  tool, `ask_user_question`, that opens a panel of up to four questions with two to four
  written options each — single-select, multi-select, a free-text `Type something.` row on
  every question, a per-question note on `n` and one global note on the submit tab. Answers
  come back in the same turn as `details.answers[]` (`kind` = `option` / `custom` / `multi`,
  plus `selected`, `notes`, `preview`), with `globalNote` and `unanswered` alongside, so the
  model can tell "not decided yet" from "settled" instead of re-asking. Reservations and
  limits are enforced before anything is shown: 1-4 questions, 2-4 options, `header` up to
  16 characters, `label` up to 60, and `Other` / `Type something.` / `Next` rejected as
  labels. Without a UI (non-interactive runs) the tool answers `no_ui` right away instead of
  waiting forever, and one questionnaire at a time is the rule — a second call is refused.
  Off by default and available in the `Dev` / `Creator` extension modes; turn it on in
  `/extension`.

- **The `ponytail` extension writes the smallest change that works**: enabling it appends a
  "lazy senior developer" ruleset to the system prompt before every request — nothing is written
  into the session, so turning it off leaves no residue in the context. The ruleset is one
  ladder: does this need to exist, is it already in this repo, does the standard library or the
  platform do it, is there an installed dependency, can it be one line, and only then the minimum
  code that works. What it never cuts: validation at trust boundaries, error handling that
  prevents data loss, security, accessibility, and anything the user asked for. New logic with a
  branch, a loop, a parser, money or security leaves one small test behind, and a deliberate
  shortcut is marked with a `shortcut: <ceiling>, <when to upgrade>` comment. Each reply ends
  with one or two lines naming what was skipped or not checked and any risk the user should know.

  The level shapes the strength: `/ponytail off` turns it off, `lite` builds what was asked and
  names the smaller option in one line, `full` (the default) applies the ladder, and `ultra`
  also pushes back on parts of the request that the need does not justify. A bare `/ponytail`
  turns it on at the default level, or reports the current level when it is already on;
  `/ponytail status` reports the session level together with the default, and
  `/ponytail default <level>` writes `defaultMode` in `agent_dir()/extensions/ponytail.json`.
  The session level lives in a session entry (`ponytail-mode`), so resuming or switching a
  session replays it.

- **Four checklists ship as `/ponytail` subcommands, a fifth prints locally**: `/ponytail review [scope]` (quality
  review of a change: bugs, security, load, missing tests, speed, what to cut — each finding
  says what the code does, what goes wrong, how to fix it and what happens if we skip it),
  `/ponytail audit [scope]` (the same review across the whole repo, ranked),
  `/ponytail debt [marker]` (harvest `shortcut:` comments into a debt ledger),
  and `/ponytail gain` (the published benchmark scoreboard). Their text is embedded in the
  binary and delivered as a `<skill>` block when invoked, so nothing is written to disk and no
  skill description leaks into the system prompt while the extension is off; a checklist sent
  while a turn is running steers into that turn. `/ponytail help` is different: its reference
  card is rendered in the terminal itself, so it costs no turn and never lands in the history.

  Off by default and available in `Dev` mode only: it rewrites how the model writes in every
  turn, so it has to be switched on explicitly.

- **`remote_default` answers which machine the remote tools hit**: the ssh-remote family gained a
  seventh (deferred) tool reporting the target used when `host` is omitted — its `user@host:port`,
  remembered cwd, note, jump host and identity, the remembered target list and the active
  forwards. It only reads the local config, so it never opens a connection and never waits on the
  network; with no default set it says so instead of guessing a host. The model can therefore stop
  asking which server to use, and users need not run `/ssh status` for it.

- **`markit` can write PDFs too**: the new `export_pdf` tool (deferred, loaded through
  `tool_search` like `read_doc`) typesets Markdown into a PDF with the external `typst` command
  (`https://github.com/typst/typst`): headings, emphasis, lists, fenced code, tables, links and
  `$math$` become Typst markup, which is compiled from a temporary directory so only the `.pdf`
  is left behind. No typesetting engine is embedded: when `typst` is not on `$PATH` the tool
  answers with that download URL and tells the model to relay it instead of retrying. When the
  source contains CJK text it asks `typst fonts` which families exist and puts one into
  `#set text(font: (..))` — Typst never falls back on its own, so without that step Chinese
  would come out as empty boxes. Variable font families are probed once with a throwaway compile
  and dropped from that list: Typst renders them as empty boxes too, and it does not fall through
  to the next family. Layout: the level-1 heading is centered, an italic paragraph right after it
  becomes a grey centered subtitle, and block quotes become tinted rounded boxes.
  Parameters: `content` and an optional `path` (`output.pdf`).

- **Classifier models can take images**: a classifier model whose catalog entry declares
  `input: ["text", "image"]` now receives pictures in the form its service expects —
  TypeSafe's `assets` map (PNG/JPEG, referenced from the state by `<image-N>`, with the
  experimental `TypeSafe-Danger-Zone-Images` opt-in header), Clef's `images` list of data
  URLs (at most four, PNG/JPEG/WebP), or OpenRouter's `image_url` parts placed ahead of the
  state. `compat.imageInput` picks the form and `compat.sendImageOptInHeader` decides on the
  opt-in header; without them the provider and `baseUrl` decide. A provider-level `compat` in
  `models.json` now also reaches classifier entries, not just chat ones, so a proxy that
  hides which service it fronts can be configured instead of guessed.

- **Every request identifies itself**: providers now get
  `User-Agent: prux/<version> (<platform>; <arch>)`, and OpenRouter and NVIDIA NIM requests
  add their attribution headers (`HTTP-Referer`, `X-OpenRouter-Title`,
  `X-OpenRouter-Categories`, and `X-BILLING-INVOKE-ORIGIN` respectively). They are the base
  layer: catalog headers, session headers and `models.json` configuration still override them.

- **Rex is a known terminal**: with `TERM_PROGRAM=rex`, inline images use the kitty protocol
  (instead of halfblocks) and the palette is treated as truecolor, so hyperlinks and pictures
  render instead of degrading to `text (url)`.

- **The desktop GUI switches models and thinking levels from the keyboard**: `Ctrl+P` /
  `Ctrl+Shift+P` step through the model scope (`--models` / `settings.enabledModels`, falling back
  to every configured model only when the scope is empty, keeping the scope's order and skipping
  entries the catalog no longer has); a scoped entry that carries a thinking level sets that level
  before switching. `Shift+Tab` cycles the current model's thinking levels. Both follow the TUI
  bindings.

- **The desktop GUI now shares the TUI's keymap**: the command palette moved to `Ctrl+M`, so `Ctrl+K`
  is free to delete to the end of the line the way it does in the terminal. Every key the two
  interfaces have in common now means the same thing: `Ctrl+O` expands tool output, `Ctrl+L` opens
  the model picker, `Ctrl+T` shows or hides thinking blocks, `Ctrl+Shift+T` toggles light/dark, and
  desktop-only panels moved to `Shift` combinations (`Ctrl+Shift+O` sessions, `Ctrl+Shift+G`
  appearance). `Ctrl+X` copies the last reply, `Alt+C` copies the input, `Ctrl+Home` / `Ctrl+End`
  jump the transcript to either end and `Shift+PageUp` / `Shift+PageDown` page it.

- **Every list in the desktop GUI answers to the keyboard**: overlays, the model / thinking /
  attachment dropdowns, `@` completions and the session right-click menu all move with
  `Ctrl+K` / `Ctrl+J` (or the arrows), jump with `Home` / `End`, page with `PageUp` / `PageDown`
  and close with `Ctrl+C`. The list scrolls to keep the selected row in view, so a tall panel never
  leaves the highlight off screen. The session list also answers `Ctrl+R` (rename), `Ctrl+D`
  (delete) and `Ctrl+G` (next session) as it does in the terminal.

- **The desktop GUI toggles its session list and browses history from the input box**: `/sidebar`
  shows or hides the left session list (the same state `Ctrl+Shift-B` flips). In the input box
  `Up` / `Down` now walk a multi-line draft line by line and only switch prompt history once the
  cursor sits on the first line at column 0 (or the last line at its end) — landing on a line
  boundary first when it is not there yet — and the draft comes back untouched when you cycle off
  the oldest entry. Browsing history never opens the completion panel, so recalling `/model`
  leaves `Up` / `Down` under your control.

- **Desktop GUI spacing follows one scale**: paddings, gaps, corner radii, font sizes and control
  heights now come from named steps in the GUI theme (multiples of 4, with a 2 px step reserved for
  gaps between adjacent rows) instead of per-call pixel literals, so a button, its neighbours and
  the panel it sits in no longer each pick their own 6, 7, 13 or 15.

- **The desktop GUI's session list spans the whole window**: the sidebar now starts at the top
  edge instead of below a title bar, which is gone entirely. The model and thinking switches moved
  to the composer's hint row, next to `+ Attach`, and the command palette still opens on `Ctrl+M`
  — only its chip is gone.

- **The desktop GUI's right panel numbers its diffs**: the diff view skips the `diff --git` /
  `index` / `---` / `+++` file header and starts at the first hunk, and every line now carries a
  gutter with its old or new line number (`+428` added, `-428` removed, ` 429` context), so a
  hunk's position is readable without counting down from its `@@` marker. Both tabs also report
  the full counts: the file list and the change list are computed whichever tab is open, so the
  tab labels no longer disagree with what the other tab shows.

- **The desktop GUI's status bar reports the run, not just the message count**: the left side shows
  messages, the share of the context window in use and the cache hit rate; the right side shows
  input / output tokens, cost, output rate in tokens per second, and whether a goal or plan is
  active, how many turns have been sent and how long the round has taken. While the agent works,
  a blinking dot and `Working… (Esc to abort, N queued)` lead the row, and a half-second redraw
  keeps the elapsed time and the blink moving even when a long tool call sends no events. The send
  button turns into a stop square during a run. Context usage comes from the estimated history
  instead of the last request, so it shows up in a freshly loaded session too.

- **The desktop GUI remembers its own light or dark theme**: `Shift+T` and the appearance panel in
  `/settings` write the choice to `theme-gui` in `settings.json` and restore it on the next
  launch, so the window theme no longer follows `--theme` once you have picked one. The terminal's
  `theme` is a separate key and is left alone.

- **The desktop GUI's skill, extension and scoped-model panels are tables**: a header row names the
  columns, and name, source / modes / provider and description each get a fixed-width cell instead
  of one run-together `description · source · [model-off]` string, so rows line up and long text is
  clipped per cell (the `Tab` detail page still shows the full path). The scoped-model panel also
  matches the other two: `Enter` toggles a checkbox in a draft and only `Ctrl-S` writes
  `enabledModels`, with the count of enabled models in the title, and `Esc` throws the draft away.
  The `Tab` detail page is a `Property / Value` table too, and a `fork url` value that is an
  `http(s)://` link opens in the system browser on click. Commands that already have a GUI panel of
  their own — `/skills`, `/extension`, `/settings`, `/scoped-models` — no longer show up in the `/`
  completion panel.

### Fixed

- **Enter in the settings panel changes the setting instead of closing the panel**: every row now
  keeps the list open (and keeps your filter and highlighted row), so you can flip several settings
  in a row. The stop square on the send button is drawn in the accent colour rather than inverted.

- **`Tab` in the desktop GUI's `/` panel completes instead of running**: it writes the highlighted
  command into the input (with a trailing space) and leaves running to `Enter`, so a completion can
  be edited first. `@` file candidates still only complete on `Enter` and never send, matching the
  terminal UI.

- **`uiScale` lands in `settings.json` as one decimal**: writing the float straight through turned
  `2.1` into `2.0999999046325684`; it is now quantised to one decimal before saving.

- **`!` and `!!` no longer open a completion panel in the desktop GUI**: shell lines run on
  `Enter`, so the panel only appeared to announce itself and then stole `Up` / `Down` from the
  shell command; the trigger and its empty-panel hint are gone.

- **Desktop GUI `/` and `@` matching now follows the terminal UI**: `/` ranks commands by
  order-of-characters matching instead of prefix (skills match by their bare name, so `/idea` no
  longer hits `skill:deep-research`), and `@` scores paths — file name exact, then file name
  prefix, then file name substring, then path substring — sorting ties by depth, length and name.
  Both use the same shared scoring code as the terminal UI.

- **Command palette entries run the command they name**: cloning a list entry mapped any command
  outside a hard-coded handful to `/session` stats, so "Share session (gist)…" only printed the
  statistics. Entries now carry their command through unchanged.

- **Dragging a sidebar edge no longer overshoots**: sidebar and right-panel widths were stored in
  scaled design pixels but mouse movement arrives in physical pixels, so at any zoom other than
  100% the panel grew faster than the pointer. The delta is now converted before it is applied.

- **Left/Right now reach the panel**: the TUI key translation never mapped `←`/`→`, so every
  overlay saw them as "some other key" — the `ask-user-question` tabs only moved on `Tab`, and
  no extension could bind horizontal navigation. Both arrows are mapped now.
- **Transient gateway, proxy and OAuth failures now retry**: `upstream_stream_read_error`,
  `stream was interrupted`, `Upstream HTTP/2 stream failed`, `stream disconnected before
  completion` and a failed Google Vertex OAuth token refresh (`request to … failed, reason:`)
  all fall through to the retry policy instead of failing the turn on the first attempt.
- **HTTP status codes must stand alone**: `429`/`5xx` used to match anywhere in the error text,
  so a request id such as `req_011CU503xyz` or a byte count such as `5242880 bytes` triggered
  pointless backoff retries. A code now only counts when it is not part of a longer word or number.
- **ChatGPT Business workspace limits are not retried**: the workspace admin has to raise that
  limit, so backing off on it only wasted turns.
- **Compaction and branch summaries use the model's output limit**: they no longer derive a cap
  from `compaction.reserveTokens` (which only decides *when* to compact), so a thinking model
  can no longer exhaust the summary budget and fail with "generation hit the token cap".
- **Any HTTP 413 counts as context overflow**: an oversized request body (for example a session
  full of large images on GitHub Copilot) now compacts and retries instead of failing.
- **Whitespace-only tool output uses the empty placeholder**: a bare `\r\n` from `bash` used to
  be sent verbatim to the OpenAI Completions, OpenAI Responses and Google APIs, which reject it
  with a 400. Google now also has a placeholder for genuinely empty output.
- **`grep` runs `rg --no-config`**: a `RIPGREP_CONFIG_PATH` in the user's shell (for example
  with `--fixed-strings`) silently turned regex searches into literal ones.
- **PowerShell is found off `PATH`**: `pwsh` in `%ProgramFiles%\PowerShell\7` and
  `powershell.exe` in `%SystemRoot%\System32\WindowsPowerShell\v1.0` are now detected.
- **An `mcp.json` that starts with a UTF-8 BOM parses**: before, the whole file failed to parse
  and every MCP server in it was silently skipped.
- **The clipboard helper leaves our process group**: `wl-copy` is spawned detached, so
  suspending prux with Ctrl+Z no longer suspends the process that owns the clipboard and makes
  every later paste hang.
- **A panic restores the terminal**: raw mode, mouse tracking, focus reporting, bracketed paste,
  the Kitty keyboard protocol and the alternate screen are now turned off by the panic hook, so
  a crash no longer floods the shell prompt with escape sequences.
- **Tools registered after the session started stay active**: resuming a session or switching
  branches used to restore exactly the recorded tool list, leaving extension tools that were
  registered later permanently inactive. Registered tools the branch never added or removed are
  now active.
- **HTML export renders CJK emphasis**: bold and italic next to CJK punctuation no longer show
  up as literal asterisks.
- **The desktop GUI's transcript and menus behave**: right-clicking a session row and picking
  `Rename` / `Delete` now opens the dialog instead of just closing the menu, the transcript can
  be scrolled to its real end (the last lines are no longer hidden under the composer), text
  selection lands on the character under the pointer, `Esc` during a turn drops the busy state
  and leaves an `Operation aborted` line behind, and command results appear in the transcript
  as system messages rather than a transient toast. UI scale can be raised to 500%.
- **A second overlay no longer replaces the first**: when two extensions ask for input at the
  same time, the second one used to take over and the first was simply gone. The covered overlay
  is now kept on a stack and comes back as soon as the one above it closes; switching sessions
  still closes all of them.

- **The desktop GUI's input box wraps to the width it really has**: wrapping measures real glyph
  widths through the text system instead of assuming 0.6em per character — the estimate overflowed
  the card once the interface was zoomed, and scaling it back wrapped a fifth too early; the
  available width is now the card's inner width. A draft longer than eight rows scrolls with the
  wheel, the cursor scrolls itself back into view when it moves without fighting a manual scroll,
  and collapsed tool output ends with the same `⋯ N more line(s) · click to expand` hint the user
  bubbles use instead of a bare `…`.
- **Streaming no longer drags the transcript back to the bottom**: every agent event used to call
  `scroll_to_bottom`, so scrolling up to read history was undone event by event. Only a view that is
  still following the bottom auto-scrolls now; reloading a session re-arms it.

- **The desktop GUI's `/extension` and `/skills` panels behave like the TUI ones**: a toggle only
  edits an unsaved draft — `Ctrl+S` writes it, `Esc` throws it away — and the panel keeps the row you
  clicked selected along with the filter box instead of jumping back to the first row after the list
  is rebuilt. `Tab` opens a read-only detail page for the selected entry (`Tab` / `Enter` / `Esc` go
  back, the draft survives) — the extension description, mode, tools and fork URL, or the skill's
  `SKILL.md` header, source and path. Footer and banner extensions are no longer listed, since only
  the terminal UI renders them.

## 中文

### 新增

- **新增 `ask-user-question` 扩展：让模型问，而不是猜**：只加一个工具 `ask_user_question`，
  弹出最多四题的覆盖层面板，每题 2–4 个写好的选项——单选、多选、每题末尾自动带一行
  `Type something.` 自由作答、`n` 写逐题备注、提交页写整份问卷的全局备注。作答在同一轮
  以 `details.answers[]` 回到模型（`kind` = `option` / `custom` / `multi`，附带 `selected` /
  `notes` / `preview`），另带 `globalNote` 与 `unanswered`，因此「还没定」和「已经定了」能被
  分开，模型不必重问。保留标签与上限在弹面板之前就校验：1–4 题、每题 2–4 个选项、`header`
  最长 16 字符、`label` 最长 60 字符，`Other` / `Type something.` / `Next` 作为标签一律拒绝。
  没有 UI（非交互运行）时工具立刻返回 `no_ui` 而不是干等；同一时刻只允许一份问卷，第二份
  会被拒绝。默认不启用（仅 `Dev` / `Creator` 扩展模式可用），在 `/extension` 面板开启。

- **新增 `ponytail` 扩展：只写「最小的完整改动」**：启用后每次请求前把一套「懒资深开发者」
  规则追加到系统提示末尾——**不写进会话**，所以关掉之后上下文里不留残渣。规则是一条梯子：
  这东西需要存在吗、仓库里已经有了吗、标准库或平台原生有没有、已装依赖里有没有、能不能一行写清，
  最后才是「能跑的最小实现」。它绝不省的东西：信任边界上的校验、防丢数据的错误处理、安全、
  无障碍，以及用户明确要求的部分。带分支 / 循环 / 解析 / 钱 / 安全的新逻辑要留一个小测试，
  刻意的简化用 `shortcut: <上限>, <何时升级>` 注释标注。每轮回复结尾用一两行点明「跳过了什么、
  没检查什么、有什么风险」。

  档位决定强度：`/ponytail off` 关闭，`lite` 按用户要求做但用一句话点名更小的做法，`full`
  （默认）走完整梯子，`ultra` 额外在动手前反驳需求里不成立的部分。裸 `/ponytail` 在关闭时按
  默认档开启、在已开启时报告当前档；`/ponytail status` 同时报本会话档位与默认档；
  `/ponytail default <档位>` 把 `defaultMode` 写进 `agent_dir()/extensions/ponytail.json`。
  本会话档位记在会话自定义条目（`ponytail-mode`）里，恢复或切换会话时重放。

- **扩展自带五个检查清单，都是 `/ponytail` 的子命令（前四个投递给模型，`help` 在本机打印）**：`/ponytail review [目标]`（改动
  审查：bug、安全、真实负载、缺测试、性能、可删的代码——每条都说明这段代码在做什么、
  哪里会出错、怎么修、不修会怎样）、`/ponytail audit [目标]`（同一套审查跑全仓，按严重度
  排序）、`/ponytail debt [标记词]`（把 `shortcut:` 注释收成债务账本）、`/ponytail gain`
  （已发布的 benchmark 记分牌）。这些正文随二进制内嵌，执行时作为 `<skill>` 块投递给模型，
  不落盘；扩展关闭时也不会有技能描述漏进系统提示。回合运行中投递清单会作为 steer 插进当前回合。
  而 `/ponytail help` 的速查卡直接在终端里渲染：不花一个回合，也不进会话历史。

  默认不启用，仅在 `Dev` 模式下可用：它改写的是模型每一轮的写作风格，必须由用户显式开启。

- **新增 `remote_default`：省略 `host` 时到底连哪台机器**：ssh-remote 工具族补上第七个（延迟）
  工具，报出默认目标的 `user@host:port`、记住的 `cwd`、备注、跳板机与私钥、已记住的目标列表、
  活跃的端口转发。它只读本地配置，不建连接、不等网络；没设默认目标时如实说「没有」，不猜主机。
  模型因此不必再问「用哪台服务器」，用户也不必为这件事跑 `/ssh status`。

- **`markit` 也能写 PDF 了**：新增 `export_pdf` 工具（**延迟工具**，与 `read_doc` 一样经
  `tool_search` 载入），把 Markdown 翻成 Typst 标记——标题、强调、列表、围栏代码、表格、
  链接、`$公式$`——再交给系统里安装的 `typst` 命令（`https://github.com/typst/typst`）编译；
  源文件写在临时目录，只在目标位置留下 `.pdf`。**不内嵌排版引擎**：`$PATH` 里没有 `typst`
  时直接回下载地址，让模型转告用户而不是反复重试。正文含中文时先跑 `typst fonts` 问系统
  装了哪些字体族，挑一个拼进 `#set text(font: (..))`——Typst 不做隐式回退，少了这步中文会
  变成空框。可变字体家族先用一次试编译探出来再排除：它们同样会渲染成空框，而且不会回退到
  列表里的下一个家族。排版上：一级标题居中，紧跟其后的整段斜体排成灰色居中副标题，引用块排成
  灰底圆角框。参数：`content` 与可选的 `path`（默认 `output.pdf`）。

- **分类器也能收图片了**：目录条目声明 `input: ["text", "image"]` 的分类器模型，现在会按所用服务的
  形态收到图片——TypeSafe 的 `assets` 映射（PNG / JPEG，文件名 `<image-N>` 在 `state` 之前引用，
  并带实验性的 `TypeSafe-Danger-Zone-Images` opt-in 头）、Clef 的 `images` data URL 列表（最多四张，
  PNG / JPEG / WebP），或 OpenRouter 排在 `state` 之前的 `image_url` 片段。形态由
  `compat.imageInput` 决定、opt-in 头由 `compat.sendImageOptInHeader` 决定，两者都缺时按 provider
  与 `baseUrl` 判断；`models.json` 的 provider 级 `compat` 也从只作用于 chat 条目扩展到分类器条目，
  藏在代理后面、看不出是哪家服务的端点因此可以配置而不是靠猜。

- **每个请求都标明自己是谁**：请求带 `User-Agent: prux/<版本> (<平台>; <架构>)`；OpenRouter 与
  NVIDIA NIM 另带归因头（`HTTP-Referer`、`X-OpenRouter-Title`、`X-OpenRouter-Categories`，
  以及 `X-BILLING-INVOKE-ORIGIN`）。这些头是最底层，目录自带头、会话头和 `models.json`
  配置仍然会覆盖同名项。

- **Rex 终端被认出来了**：`TERM_PROGRAM=rex` 下内联图片走 kitty 协议（不再退化成半块字符），
  配色按 truecolor 处理，超链接与图片都能正常显示，而不是退成 `text (url)`。

- **桌面 GUI 也能只用键盘切模型与思考级别**：`Ctrl+P` / `Ctrl+Shift+P` 在模型 scope 里前后循环
  （scope 来自 `--models` / `settings.enabledModels`，为空时才退回全部已配置模型；顺序跟 scope、
  跳过目录里已经没有的条目，条目带 thinking 时先设思考级别再换模型）；`Shift+Tab` 循环当前模型
  支持的思考级别。两个键都与 TUI 的默认绑定一致。

- **桌面 GUI 的键位与 TUI 统一了**：命令面板改到 `Ctrl+M`，`Ctrl+K` 因此回到「删到行尾」。两边共有的
  键含义一律对齐：`Ctrl+O` 展开工具输出、`Ctrl+L` 打开模型选择、`Ctrl+T` 显隐思考块、`Ctrl+Shift+T`
  切明暗；桌面独有面板挪到 `Shift` 组合（`Ctrl+Shift+O` 会话、`Ctrl+Shift+G` 外观）。新增 `Ctrl+X`
  复制最后一条回复、`Alt+C` 复制输入框、`Ctrl+Home` / `Ctrl+End` 跳到对话流首尾、
  `Shift+PageUp` / `Shift+PageDown` 翻页。

- **桌面 GUI 的每个列表都认键盘**：覆盖层、模型/思考级别/附件下拉菜单、`@` 补全候选、会话右键菜单
  都能用 `Ctrl+K` / `Ctrl+J`（或方向键）上下移动、`Home` / `End` 跳首尾、`PageUp` / `PageDown` 翻页、
  `Ctrl+C` 关闭；选中行超出视口时列表会跟着滚，高面板里高亮不会跑到屏幕外。会话列表还原了
  终端里的 `Ctrl+R`（改名）、`Ctrl+D`（删除）、`Ctrl+G`（下一个会话）。

- **桌面 GUI 能开关会话列表，输入框也能翻历史**：`/sidebar` 显示或收起左侧会话列表（与
  `Ctrl+Shift-B` 同一状态）。输入框的 `↑` / `↓` 会先逐行走过多行草稿，只有光标在第一行行首
  （或最后一行行尾，未到位时先归位）才切到提示历史；从最早一条再往上绕回草稿，草稿原样恢复。
  浏览历史时不弹补全面板，翻出 `/model` 这类历史也不会把 `↑` / `↓` 抢走。

- **桌面 GUI 的间距统一成一套刻度**：padding、gap、圆角、字号与控件高度改用 GUI 主题里命名的
  阶梯值（4 的倍数，只给相邻行之间的细缝留了 2px 一档），不再各处自己写像素字面量——按钮、
  它旁边的元素和所在面板不会再各自取 6、7、13、15。

- **桌面 GUI 的会话列表占满整列**：侧栏从窗口顶端开始，不再有压在它右侧的标题栏——标题栏
  整个去掉了。模型与思考级别切换挪到输入卡片的提示行、`+ Attach` 旁边；命令面板照旧用 `Ctrl+M`
  打开，只是那个芯片没了。

- **桌面 GUI 的右侧面板给 diff 标上行号**：diff 视图跳过 `diff --git` / `index` / `---` / `+++`
  这些文件头，直接从第一个 hunk 开始渲染，每行左侧带旧/新行号（新增 `+428`、删除 `-428`、
  上下文 ` 429`），不必从 `@@` 标记往下数。两个页签的计数也都齐了：无论停在哪个页签，文件
  列表与变更列表都会算一遍，标签页上的数字不再和另一个页签对不上。

- **桌面 GUI 的状态栏汇报这一轮，而不只是消息数**：左侧是消息数、上下文窗口占用率与缓存命中率，
  右侧是 in/out token、费用、每秒输出速率，以及当前是否有目标或计划、已发交互数与本轮耗时。
  忙碌时行首先亮一个闪烁圆点与 `Working… (Esc to abort, N queued)`，并每半秒重绘一次，长工具
  调用期间没有事件时耗时也照样走；发送按钮在忙碌时变成停止方块。上下文占用改用历史估算而不取
  最后一次请求，刚载入的会话也能看到占用率。

- **桌面 GUI 记住自己选的明暗主题**：`Shift+T` 与 `/settings` 的外观面板把选择写进 `settings.json`
  的 `theme-gui`，下次启动照样恢复，选过一次之后窗口主题就不再跟着 `--theme` 走；终端里的 `theme`
  是另一个键，互不影响。

- **桌面 GUI 的技能、扩展与模型名单面板改成表格**：有表头行，名称、来源（扩展是可用模式、模型是
  provider）与描述各占一列定宽，不再把 `描述 · 来源 · [model-off]` 拼成一串挤在行尾，行与行对得齐，
  长文本按单元格截断（`Tab` 详情页仍显示完整路径）。模型名单面板也跟另外两个对齐：`Enter` 只切换
  草稿里的勾选框、只有 `Ctrl-S` 才写回 `enabledModels`，标题里带启用数量，`Esc` 丢弃草稿。`Tab` 详情页
  同样是 `Property / Value` 两列表格，`fork url` 这类 `http(s)://` 链接点一下就交给系统浏览器打开。
  已有独立 GUI 面板的命令——`/skills`、`/extension`、`/settings`、`/scoped-models`——不再出现在 `/`
  补全面板里。

## 修复

- **设置面板按 Enter 只改配置、不关面板**：每一行都保留列表（过滤串与高亮行一起留着），可以连着
  改好几项。发送按钮上的停止方块改用品牌强调色，而不再取反色。

- **桌面 GUI 的 `/` 面板里 Tab 只补全、Enter 才执行**：Tab 把选中的命令连同尾随空格写进输入框，
  执行留给 Enter，因此补全完还能先改；`@` 文件候选仍然只在 Enter 时补全、不发送，与终端一致。

- **`uiScale` 在 `settings.json` 里只留一位小数**：直接把 f32 转过去会写成 `2.0999999046325684`，
  现在存盘前量化到一位小数。

- **桌面 GUI 里 `!` / `!!` 不再弹出补全面板**：shell 行按 `Enter` 就执行，面板只是自报一句说明，
  却把 `↑` / `↓` 从 shell 命令手里抢走；触发词与那条空面板提示一并去掉。

- **桌面 GUI 的 `/` 与 `@` 匹配规则跟终端对齐**：`/` 改成按字符顺序匹配并排序（技能按裸名参与，
  `/idea` 不会再命中 `skill:deep-research`），`@` 按路径打分——文件名完全匹配、文件名开头、
  文件名子串、仅路径子串——同分再按层级、长度、字典序。两边共用同一份打分代码。

- **命令面板执行的就是它写着的那条命令**：拷贝列表项时，硬编码清单之外的命令会被换成 `/session`
  统计，于是「Share session (gist)…」只打印了统计。现在命令原样带过去。

- **拖侧栏边框不再跑偏**：侧栏与右侧面板的宽度按「设计像素 × 缩放」存，鼠标位移却是物理像素，
  缩放不是 100% 时面板比指针长得快。现在位移先折回宽度单位再相加。

- **`←` / `→` 现在能送到面板**：TUI 的按键翻译从来没映射左右方向键，所有覆盖层都把这两个键当成
  「其它键」——`ask-user-question` 的标签页只有 `Tab` 能翻，扩展也无法绑定横向导航。现在两个方向键
  都会映射过去。
- **网关、代理与 OAuth 瞬时故障会重试了**：`upstream_stream_read_error`、`stream was interrupted`、
  `Upstream HTTP/2 stream failed`、`stream disconnected before completion`，以及 Google Vertex
  的 OAuth token 刷新失败（`request to … failed, reason:`）现在都走重试策略，不再第一下就判死。
- **HTTP 状态码必须独立成 token**：`429` / `5xx` 原来在错误文案里任意位置都命中，`req_011CU503xyz`
  这类请求 id、`5242880 bytes` 这类字节数都会触发无谓的退避重试；现在前后是字母或数字就不算状态码。
- **ChatGPT Business 的工作区上限不再重试**：那种额度只有管理员能提，退避重试只是浪费回合。
- **压缩与分支摘要改用模型的输出上限**：不再从 `compaction.reserveTokens` 推一个输出额度（那个值只决定
  **何时**开始压缩），思考型模型不会再把摘要额度吃光后报 "generation hit the token cap"。
- **任何 HTTP 413 都算上下文溢出**：请求体过大（例如 Copilot 上塞满大图的会话）会先压缩再重试，
  而不是直接失败。
- **只有空白的工具输出改用空输出占位文案**：`bash` 吐的一个裸 `\r\n` 原来原样发给 OpenAI Completions、
  OpenAI Responses 与 Google，被这三家以 400 拒掉。Google 侧此前连真空输出都没有占位文案，一并补上。
- **`grep` 固定走 `rg --no-config`**：用户 shell 里的 `RIPGREP_CONFIG_PATH`（例如带 `--fixed-strings`）
  会把正则搜索静默改成字面量搜索。
- **PowerShell 不装在 `PATH` 上也能找到**：`%ProgramFiles%\PowerShell\7` 下的 `pwsh` 与
  `%SystemRoot%\System32\WindowsPowerShell\v1.0` 下的 `powershell.exe` 都纳入探测。
- **带 UTF-8 BOM 的 `mcp.json` 能解析**：此前整份文件解析失败，里面的 MCP server 全部被静默跳过。
- **剪贴板子进程脱离我们的进程组**：`wl-copy` 以独立进程组启动，`Ctrl+Z` 挂起 prux 时不会连带把它停掉，
  之后的粘贴不会卡住。
- **panic 时还原终端**：raw mode、鼠标跟踪、focus 上报、括号粘贴、Kitty 键盘协议与备用屏现在由 panic
  钩子关掉，崩溃退出不再把 shell 提示符糊满转义序列。
- **会话开始后才注册的工具保持激活**：恢复会话、切换分支原来只装回记录的名单，后来注册的扩展工具
  永远是非激活态；分支既没加过也没删过的已注册工具现在默认激活（声明 `default_active: false` 的工具不在此列，
  仍要用户显式开启）。
- **HTML 导出的 CJK 强调正常了**：CJK 标点紧邻 `**` / `*` 时不再显示字面星号。
- **桌面 GUI 的对话流与菜单修好了一批问题**：会话行右键的 `Rename` / `Delete` 能打开对话框
  （之前只把菜单关掉），对话流能滚到真正的底部（最后几行不再被输入卡片盖住），文本选区的落点
  对准指针下的字符，回合中按 `Esc` 会退出忙碌状态并在对话流里留下 `Operation aborted`，
  命令结果作为系统消息写进对话流而不是只弹 toast；界面缩放上限提到 500%。
- **第二个覆盖层不再顶掉第一个**：两个扩展同时要输入时，第二个会把第一个挤掉、前者直接消失；
  现在被盖住的覆盖层挂在栈上，上面的关掉就露回来，切换会话仍然会一次关掉全部。
- **桌面 GUI 的输入框按真实字宽折行，长草稿也能滚**：折行改成用排版器量每个字符的实际宽度，
  不再按 0.6em 估算（估算在放大后会溢出卡片，缩放一次又会早折一截），可用宽度也改用卡片的真实
  内宽；超过八行的草稿能用滚轮滚，光标移动时自己滚进视口、但不会跟手动滚动抢位置；折叠的工具
  输出不再只留一个 `…`，改成和用户气泡一样的 `⋯ N more line(s) · click to expand`。
- **流式输出不再把对话流拽回底部**：原来每来一条 agent 事件都调用 `scroll_to_bottom`，用户滚上去
  看历史会被逐条拽回底部；现在只有仍跟随底部时才自动滚，重载会话会重新接管。
- **桌面 GUI 的 `/extension` 与 `/skills` 面板跟 TUI 对齐**：勾选只改未落盘的草稿，`Ctrl+S` 才写盘、
  `Esc` 丢弃；点过哪一行就是哪一行，列表重建后不再跳回第一条，过滤框也一起保留。`Tab` 打开当前条目
  的只读详情页（`Tab` / `Enter` / `Esc` 返回，草稿还在）——扩展显示描述、模式、工具与 fork 来源，
  技能显示 `SKILL.md` 头信息、来源与路径。底栏与横幅扩展不再列出，它们只在终端界面渲染。
