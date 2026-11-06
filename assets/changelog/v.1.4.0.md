## English

### Added

- **New desktop GUI: `prux-gui`**: the same agent in a window. It shares `agent_dir()`,
  `settings.json`, the session files and the extensions with the terminal UI, so the model chain,
  the tools and the conversation are identical and one session can be opened in either one. Three
  columns: the session sidebar on the left (search, grouped by activity, rename / delete,
  collapsible and resizable, branch and working directory at the bottom), the conversation in the
  middle (Markdown, tables, formulas, mermaid diagrams, images, diffs with line numbers, tool cards
  with elapsed time, queued messages, the dock panel, a multi-line input card and a one-line status
  bar), and the Files / Changes panel on the right (collapsible, diffs with line numbers, both
  scroll axes). Slash commands, `@` path completion, model and thinking level, skills / extensions
  / settings, `/login` and project trust, `/rewind`, and the questions, overlays and
  per-extension settings panels that extensions raise all work in the window; extension commands
  are written once and share one registry between the two hosts. The keymap is fixed and matches
  the terminal's meaning (it does not read `keybindings.json`), every list and overlay is
  keyboard-reachable, and the mouse selects and copies, pastes on middle click and drags panel
  widths; theme, scale, sidebar state and the self-drawn title bar (`gui.frame`) live under `gui`
  in `settings.json`, and settings toggles (hiding thinking blocks included) apply as soon as
  they change. `prux-gui --tui` starts the terminal interface from the same binary, and
  `docs/gui.md` covers the rest.

- **Panels name the keys they answer to**: the session picker now says `Ctrl+R rename · Ctrl+D delete`
  (previously only "Enter confirm"), and the logout list says what Enter does before it
  clears a key.

- **`prux acp` serves the Agent Client Protocol so an editor can drive the same agent**: the new
  `acp` extension injects an `acp` subcommand — `bootstrap()` hands it the fully assembled agent,
  so the editor gets the same model chain, tool set, session file and extension hooks as the TUI,
  not a second agent loop. Over NDJSON + JSON-RPC 2.0 on stdin/stdout it answers `initialize`,
  `authenticate`, `session/new`, `session/prompt` and `session/cancel`, streaming text, thinking
  and tool-call updates as `session/update` notifications, and a prompt keeps listening on stdin
  while it runs, so a cancel lands immediately instead of queueing behind a slow turn. When the
  client advertises `clientCapabilities.elicitation.form`, `ask_user_question`'s questionnaire goes
  out as an `elicitation/create` form the client renders; clients without it (Zed 1.3.6 sends no
  `elicitation` capability) fall back to one `session/request_permission` per single-select
  question and report multi-select questions as unanswered, while other blocking choice panels
  (such as the permission gate) always use `session/request_permission`. Single session
  (`session/new` returns the session `bootstrap()` already opened), no `session/load`, text
  prompts only; the extension registers no tools and no hooks — without `prux acp` it does
  nothing. It is on by default but only in the `Dev` / `Creator` / `All` extension modes: in
  `Minimal` the subcommand does not exist.

- **`prux --help` lists the subcommands extensions inject**: `prux acp` and `prux mcp` are now
  real clap subcommands, injected from the extension that owns them, so they show up in the
  Commands section with a one-line summary — and only while that extension is enabled. `mcp`'s
  subcommand moved out of the built-in CLI and into the `mcp` extension (which is off by
  default), so `prux mcp ...` now requires enabling that extension; `prux acp --help` prints its
  own usage.

- **`vet` runs only the checks the diff touches**: with `diffScoped` on (the default) the settle
  path reads `git status --porcelain` first and keeps only the checks matching the languages that
  changed — a Rust-only edit runs the `cargo …` checks, a docs-only edit runs nothing, and a
  change to a manifest or lock file runs everything. Commands whose language cannot be read
  (`just check`, `cargo test && pnpm test`) always run — skipping them would be a fake pass — and
  a non-git directory falls back to the full set. `/vet run` still runs everything.


- **A `hooks` extension runs your own commands at lifecycle points**: four events —
  `PreToolUse`, `PostToolUse`, `UserPromptSubmit` and `Stop` — are driven from
  `extensions/hooks.json` in the agent directory, in the same shape Claude Code uses, so an
  existing config drops in. Each
  hook gets one line of JSON on stdin (`hook_event_name`, `session_id`, `cwd`, plus
  `tool_name` / `tool_input` / `tool_response` / `prompt` / `stop_hook_active`) and answers with
  an exit code plus optional JSON on stdout: exit `2` blocks with stderr as the reason, exit `0`
  reads `decision: block` / `approve`, `hookSpecificOutput.permissionDecision`
  (`allow` / `ask` / `deny`), `additionalContext` and `updatedInput`. So a hook can deny a call,
  raise an approval panel, rewrite the tool input, replace what the model sees as the tool
  result, reject a prompt, or ask the agent to keep going after it tried to stop (`Stop` is
  capped at 5 consecutive continuations). Verdicts merge most-severe-first, the first `deny`
  stops the batch, `matcher` is a case-insensitive tool-name regex, each command carries its own
  timeout (10s default) and is killed with its process tree on expiry — a broken hook prints one
  warning and never blocks the call. `/hooks` lists the active hooks and any config errors,
  `/hooks settings` prints the same listing and `/hooks reload` drops the cached config and
  re-reads the file (hand edits are picked up anyway, the config is cached by mtime and length).
  Off by default (`Dev` / `Creator`).

- **An `rtk` extension compacts bash commands through the external `rtk` CLI**: every `bash`
  call runs `rtk rewrite <command>` first and executes the compact equivalent when rtk offers one
  (exit `0` or `3` with output; exit `1` means "no filter"), which keeps tool output — and the
  context it eats — small. It is fail-open by design: a missing binary, a timeout (5s), empty
  output or an exit code it does not recognise all run the original command. What it does not do
  is stay quiet: enabling it without `rtk` in `PATH` reports an error once, with the download
  address (https://github.com/rtk-ai/rtk). The rewrite happens before permission checks, so
  `permissions` and the sandbox judge the command that actually runs. Off by default
  (`Dev` / `Creator`).

- **A `rollback` extension puts back the files a turn changed**: files are snapshotted right
  before `edit` / `write` touches them and grouped by **turn** (one user message = one turn).
  `/rollback` lists the turns that changed files, `/rollback diff <n>` shows which files a turn
  touched, and `/rollback <n>` puts those files back the way they were when the turn started.
  Two details matter: a file that did not exist yet is *deleted* rather than left empty, and the
  current content is snapshotted before it is overwritten — so a mistaken rollback can itself be
  rolled back. Snapshots live in `agent_dir()/checkpoints/<workspace-key>/` (one bucket per
  working directory, 500 entries before the oldest 20% is pruned); `/rollback` refuses to run
  while a turn is in progress. It is deliberately **not** part of `rewind`: that one only moves
  the session branch back, and asking for a one-line rewind should not silently buy a file copy
  on every single write. A project under git usually needs only `git restore`; this covers what
  git cannot — content that was still uncommitted when the turn started. Off by default
  (`Dev` / `Creator`).

- **A `worktree` extension moves the session into a git worktree copy**: `/worktree enter
  [name]` creates a snapshot of `HEAD` under `agent_dir()/extensions/worktrees/<repo>-<name>` and switches
  the session's working directory into it (a caller sitting in a subdirectory keeps the same
  nesting inside the copy); `/worktree exit` commits whatever the copy changed onto a
  `prux-wt-<name>` branch (timestamp suffix when that branch already exists), deletes the copy and
  switches back; `/worktree drop` deletes it and throws the changes away; bare `/worktree` reports
  the current state. Three rules are deliberate: uncommitted changes in the working directory do
  **not** travel into the copy (they stay in the original repository, and entering says so), a
  failed `git commit` **keeps the copy** and reports the error instead of deleting the work, and
  the switch is refused while a run is in progress. Commits inside the copy use `--no-verify` —
  your hooks do not run in a throwaway copy — and a copy that cannot be deleted falls back to
  `git worktree prune`. Commands only, no tool: changing directories is the user's call, and a
  tool call has no write access to the session `cwd`. Off by default (`Dev` / `Creator`).

- **`permissions` gates network hosts too**: a fifth layer, `domain`, matches the host of every
  `http(s)://` URL a call would reach — every command line bash runs (including ones inside
  `$(…)`) and any URL found anywhere in another tool's arguments, which covers MCP calls that
  keep the URL in `arguments.url`. Hosts are matched lowercased, with an optional `host:port`
  spelling (`localhost:8080`) and bracketed IPv6. `{"*": "allow", "*.internal": "deny"}` is
  enough to keep the agent inside the intranet; leaving the layer out changes nothing, so
  outbound traffic stays unrestricted by default. `ask` goes through the same approval panel as
  the other layers, and `/permissions allow domain "*.evil"` grants one pattern for the session.

- **A `papers` extension searches the literature and fetches full texts**: `web_search` gets
  you web pages, never "which papers, and which one's actual text". Four zero-auth sources are
  queried concurrently — alphaXiv (community votes, arXiv only), OpenAlex (citation counts,
  DOIs, cross-publisher), bioRxiv (OpenAlex narrowed by source) and PubMed (two-step: rank by
  relevance, then `efetch`) — and merged by arXiv id / DOI / OpenAlex id / PMID, keeping the
  larger vote and citation counts. `paper_search` takes `query`, `sources`, `num_results`,
  `published_after` / `published_before` and alphaXiv's `prioritize`
  (`default` / `recency` / `popular` / `historical`); one failing source appends a note and the
  rest still return. `paper_get` routes a bare or prefixed arXiv id / DOI / OpenAlex id / PMID
  to the right metadata source, then walks the body chain: alphaXiv markdown full text →
  its generated overview → arXiv PDF (`export.arxiv.org`) → open-access PDF from OpenAlex.
  PDFs only yield their text layer, so a scanned paper says so instead of returning nothing.
  Bodies are truncated at 20000 characters with an `offset` hint for the next call. Both tools
  are deferred behind `tool-search`; off by default (`Dev` / `Creator`).

- **An `injection-guard` extension warns when a tool result reads like a prompt injection**: an
  agent feeds untrusted text into its own context all day — fetched pages, parsed PDFs, remote
  file reads, `curl` output — and a line of "ignore the previous instructions" inside it is
  exactly what a model is most likely to obey. Every tool result is now scored against a pattern
  table: instruction overrides, system-prompt dumping, role hijacks, chat template tokens
  (`<|im_start|>`, `[INST]`, `<<SYS>>`), line-leading role markers, `new instructions:`,
  `do not tell the user` (now also `don't` / `never`, and `them` / `anyone` as its object — the
  spelling variants used to read as one weaker rule), data exfiltration, and instructions hidden
  in HTML comments. A score of 3 or more posts one warning in the chat area (not persisted, never
  sent to the model). Obfuscation is undone before scoring: zero-width / bidi / tag characters are
  stripped and base64 / hex payloads are decoded (text-like decodes only, with caps on run length
  and payload count), so `ig\u{200B}nore` or a base64-encoded sentence still scores. Two
  exfiltration-lure rules ship with it: a secret-shaped value in a URL query (`?token=…`,
  `?api_key=…`; markdown images included — they fire on render, no click needed), and a "read a
  sensitive path and send it somewhere" directive, which needs the read verb, the path and the
  transport verb inside one sentence (`.ssh/`, `id_rsa`, `authorized_keys`, `.aws/credentials`,
  `.npmrc`, `.netrc`, `.git-credentials`, `.kube/config`, `.docker/config.json`, `/etc/shadow`).
  The result itself is **never rewritten** — redaction would break diffs and code blocks, and the
  model needs the original text to judge it. The same tool result is reported once. Off by
  default (`Dev` / `Creator`).

- **A `hardware` extension searches local datasheets and talks to real boards**: drop datasheets
  converted to Markdown (and pinout CSVs) under
  `agent_dir()/extensions/hardware/datasheets/`, grouped by chip if you like, and the
  `hardware_search` tool scores every chunk with the same lexical term splitting the rest of the
  harness uses, returning `file:line` plus the excerpt. A hit in the path weighs that chip up, so
  searching `usart` prefers `stm32f4/…` over an unrelated file; `path` narrows to prefixes and
  `limit` caps the result, and the directory is rescanned per call (no index cache). Pin aliases
  need no second mechanism — one CSV line (`PA9,USART1_TX,TIM1_CH2`) is a chunk, searchable by
  either name.

  The same extension drives boards. `probe-rs` gives `hardware_board_info` (chip and
  architecture), `hardware_memory_map` (RAM / Flash ranges) and `hardware_memory_read` (a hex dump
  of any address range); without a probe the first two fall back to a built-in board table
  (`nucleo-f401re`, `nucleo-f411re`, `arduino-uno`, `arduino-mega`, `arduino-uno-q`, `esp32`,
  `rpi-gpio`), so "what chip is this board" needs no hardware. Pins go through `gpio_read` /
  `gpio_write` / `hardware_capabilities`: either a line-delimited JSON bridge on the serial port,
  or the Uno Q Bridge on `127.0.0.1:9999`; `gpio_rpi_*` and `rpi_system_info` hit a Raspberry Pi's
  BCM pins directly (`rppal`, Linux only). `hardware_flash` writes `.elf` / `.hex` / `.uf2` /
  `.bin` over the probe and `pico_flash` copies a UF2 onto a Pico's bootloader volume — both
  destructive, therefore gated by `permissions` when it is enabled. `arduino_upload` shells out to
  `arduino-cli`, and `device_read_code` / `device_write_code` / `device_exec` go through
  `mpremote` for MicroPython boards. Serial paths are whitelisted (`/dev/ttyACM*`, `/dev/ttyUSB*`,
  `/dev/tty.usbmodem*`, `COM*`) and every call reopens the port. Config lives in
  `agent_dir()/extensions/hardware/config.json` (`boards`, `chip`, `devices`, `baud`). All tools
  are deferred behind `tool-search`; `/hardware` prints the directory, board config and tool list.
  Off by default, `Dev` mode only.

- **A `leak-guard` extension flags credentials before they leave**: the `permissions` extension
  gates *which* tools may run, not *what* leaves — a `curl -H "Authorization: Bearer …"` or a
  `.env` pasted into a prompt went out untouched. `leak-guard` scans both sides: every message's
  text block before each request, and every tool call's arguments. The rule table covers
  OpenAI / Anthropic / GitHub / GitLab / AWS / Slack / Google / Stripe keys, JWT-shaped tokens,
  private key blocks, and any run of 32+ characters that mixes case and digits with Shannon
  entropy of at least 4 bits/char (pure hex is skipped, so commits and content hashes stay
  quiet). Paths do not join into one candidate and machine-generated identifiers (UUIDs, hashes,
  `2026-11-02T10-25-54-666Z_<uuid>` session names) are skipped, and the warning memory drops its
  oldest entry instead of wiping itself — a history full of such names used to reset it every
  request and repeat the same warning every turn. Each warning names the snippet that tripped it
  (a head/tail preview such as `high-entropy: q8Zr2NvT…Zx4Q`); a refused tool call still reports
  only the category, because that reason goes back into the model context. Mode `warn` (the
  default) only notifies; `block` replaces prompt hits with `[REDACTED:<category>]` in that one
  request and refuses a tool call that carries one, telling the model which category tripped it.
  Thinking blocks are scanned but never rewritten — their text is signature-bound. A repeated
  `(category, snippet)` pair notifies once. Mode lives in
  `agent_dir()/extensions/leak-guard.json`: `/leak-guard warn|block|off` sets it, a bare
  `/leak-guard` opens the extension's settings panel. Off by default (`Dev` / `Creator`).

- **A `processes` extension supervises background jobs**: the built-in `bash` tool only runs
  commands in the foreground and kills them on timeout, so a dev server or a watcher blocks the
  whole turn. `background_bash` starts a command in the background and returns `job-N`, its pid
  and the log path at once — stdin is `/dev/null`, and stdout/stderr go to that log file instead
  of the terminal. `read_process_output` returns an output snapshot (status, pid, elapsed time
  and the last N lines; reading does not consume the log, and omitting `id` lists every job),
  while `stop_process` kills the whole process group. `/ps` toggles the dock section (three-stage,
  like `/todos`), `/ps list` prints the job list, `/ps stop <id>` stops a job and `/ps clear`
  drops finished ones. The dock section lists only running jobs (freshly finished ones are removed
  on the next frame, and the section hides itself once none are left). A job keeps running after
  the turn ends, and disabling the extension stops every running job. Off by default (`Dev` mode).

- **A bouncing-ball startup banner**: a new `bouncyballs-banner` extension drops the logo art in
  as bouncing balls — every non-blank art cell falls from above on an ease-out-bounce curve,
  turns into its real glyph on landing, and the block settles into exactly the static art. The
  right-hand info column, the divider and the three width tiers are shared with the plain
  banner, which is still selectable in `/extension` (the two are mutually exclusive; the
  animated one wins by default). Banner extensions can now ask for redraws, so the animation
  advances on the TUI's 80ms tick instead of freezing on a half-fallen frame.

- **The `ask-user-question` extension lets the model ask instead of guess**: it adds one
  tool, `ask_user_question`, that opens a panel of up to four questions with two to four
  written options each — single-select, multi-select, a free-text `Type something.` row on
  every question, a per-question note on `n` and one global note on the submit tab. Answers
  come back in the same turn as `details.answers[]` (`kind` = `option` / `custom` / `multi`,
  plus `selected`, `notes`, `preview`), with `globalNote` and `unanswered` alongside, so the
  model can tell "not decided yet" from "settled" instead of re-asking. Reservations and
  limits are enforced before anything is shown: 1-4 questions, 2-4 options, `header` up to
  16 characters, `label` up to 60, and `Other` / `Type something.` / `Next` rejected as
  labels. The submit button says `Submit` — `Submit answers` squeezed the description in a
  narrow panel — and is as wide as `Cancel`. Without a UI (non-interactive runs) the tool
  answers `no_ui` right away instead of waiting forever, and one questionnaire at a time is the
  rule — a second call is refused. Off by default and available in the `Dev` / `Creator`
  extension modes; turn it on in `/extension`.

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

  Five checklists ship as subcommands: `/ponytail review [scope]` (quality review of a change:
  bugs, security, load, missing tests, speed, what to cut — each finding says what the code does,
  what goes wrong, how to fix it and what happens if we skip it), `/ponytail audit [scope]` (the
  same review across the whole repo, ranked), `/ponytail debt [marker]` (harvest `shortcut:`
  comments into a debt ledger) and `/ponytail gain` (the published benchmark scoreboard) are
  delivered to the model as a `<skill>` block, embedded in the binary, never written to disk, and
  a checklist sent while a turn is running steers into that turn; `/ponytail help` renders its
  reference card in the terminal instead, so it costs no turn and never lands in the history.
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

- **`computer_crop` zooms into an image you already have**: the `computer` extension gains a
  fifteenth tool that crops an image file on disk to a `[x, y, width, height]` rectangle and
  returns the result, so a screenshot no longer has to be recaptured just to look closer at one
  part of it. The pixels come from the saved file, so text stays sharp even when what was
  delivered to the model was downscaled; a rectangle that does not fit inside the image is
  refused with its real size, and the reply names the offset so a cropped point can be mapped
  back to a desktop coordinate.

- **Rex is a known terminal**: with `TERM_PROGRAM=rex`, inline images use the kitty protocol
  (instead of halfblocks) and the palette is treated as truecolor, so hyperlinks and pictures
  render instead of degrading to `text (url)`.

- **`/changelog` renders as Markdown, and information overlays scroll by dragging**: the update
  log is laid out like an assistant reply — headings, lists, inline code and code blocks instead of
  one flat block of text — and the thin bar in it, in `/hotkeys` and in the detail page of list
  overlays is draggable, with a wider hit area than the 4 px it draws and a drag that keeps
  working when the pointer leaves the track. `↑` / `↓` / `PgUp` / `PgDn` / `Home` / `End` move the
  same panels.

- **The `browser` extension gained tabs, history, hovering, page state, console output and
  dialogs**: `back`, `forward` and `reload` walk the tab's own navigation history instead of
  re-navigating by hand, and `close_tab` closes a tab by index or target id. `hover` moves the
  pointer without clicking (drop-downs and tooltips need it), `dblclick` sends two click rounds,
  and `inspect` reads one piece of element state — `value`, `text`, `html`, `attr`, `count`,
  `visible`, `enabled`, `checked` — so the model no longer has to parse the snapshot text for it.
  `snapshot` takes a `scope` selector and a `depth` cap, `wait` accepts a URL fragment or an
  arbitrary JS expression, and `press` understands chords such as `Control+a` and `Meta+v`.
  Attaching a tab also turns on `Runtime` / `Log` / `Page` event reporting, buffered per tab (200
  lines): the `console` action drains that buffer — `console.*` output, uncaught exceptions,
  failed resource loads — and the `dialog` action reports an open `alert` / `confirm` / `prompt`
  and accepts or dismisses it, so a command that times out while a dialog is open says so instead
  of looking like a dead connection. `click` and `dblclick` check the element's center point first
  and refuse when another element covers it (naming it, e.g. `covered by <div#banner>`), when the
  element is disabled, or when it has no clickable area.

- **`/session` takes subcommands, including three that delete sessions**: `/session statistics` is
  the session summary that bare `/session` already printed, and `/session rm-current`,
  `/session rm-project` and `/session rm-all` delete the current session, every session of the
  current project, or every session of every project. Each one asks for a Yes/No confirmation that
  names the number of session files and the directory they live in, and each reports how many
  deletions succeeded. `rm-current` starts a new session afterwards, because the file it just
  deleted would otherwise be recreated by the next write. A deleted session goes to the trash
  rather than being unlinked outright, and the per-project folder goes away with the last session
  in it (`/new` and the desktop sidebar's `+` also stopped pre-creating it), so the directory
  listing matches what is actually on disk. `/session <n>` still switches to the n-th session, so
  existing habits keep working, and the subcommand names complete in the input box.

- **A `sandbox` extension puts shell commands inside an OS boundary**: `permissions` decides before
  execution and has to guess at dynamic command construction; this one hands the filesystem rule to
  the kernel. Enabling it probes the backend (Linux `bwrap`, macOS `sandbox-exec`) and actually
  starts a `true` process with it — bubblewrap installs fine in containers where user namespaces are
  disabled and only fails at spawn. Every `bash` call then runs with the whole filesystem read-only,
  the current working directory plus the system temp directory writable, and the network open, so
  `~/.ssh` is out of reach while `~/.cargo` also becomes unwritable: point cache directories at the
  workspace to keep builds working. A failed probe does **not** fall back to unsandboxed execution —
  it installs a wrapper that refuses every command with the reason, so the failure is visible
  instead of silent; turning the extension off is the explicit way back. `/sandbox` prints the
  backend, the writable paths and the network posture. Off by default.

- **`/debug` explains the session and names the settings keys prux does not recognize**:
  `/debug receipts` answers "what did this session actually do?" by reading the records the
  session already keeps, line by line — `tool_started` records for every call, `toolResult`
  messages for exit codes and the `edit` patch that gives exact added/removed line counts, and
  `operation_finished` for turn-level failures — plus the `permissions` review log for the calls
  that **never ran** (`deny` / `ask`), which leave no trace in the session. The receipt leads with
  totals (files changed, commands run, MCP calls, agents, failures, approvals) and then lists the
  turn in order; `/debug receipts <n>` raises the detail cap, and parsing is per line because the
  command is busy-safe: a half-written tail from the running turn is skipped, not fatal.
  `/debug settings-key` names the settings keys prux does not recognize — a `settings.json` that
  parses fine but carries a typo (`"compactReserveToken"`) used to be ignored silently, so the
  value never took effect. It lists every unknown top-level key with its `file:line` and, when a
  known key is within two edits, a `did you mean "…"?` hint; nothing to report prints a single
  confirmation line. Keys that only exist in the global file (`theme`, `skills`, …) are flagged
  when they appear in `.prux/settings.json`, which only honours `defaultTools` /
  `disabledExtensions` / `enabledExtensions`. Grammar errors stay with the startup warning; each
  run re-reads both files.

- **The `delivery` extension is now `pr`, and `/pr run` / `/pr new` hand a change off**: where
  `vet` only proves the local checks pass, `pr` does the rest of the mechanical work — branch,
  commit, push, open a pull request, then report the CI and review result. There is no
  settle-boundary hook and no feedback loop: nothing is committed or pushed behind your back, so a
  broken turn never becomes a pull request on its own. Failing checks (`name — conclusion`) and
  `CHANGES_REQUESTED` reviews are listed with the PR link; fix, then `/pr run` again — the change
  lands on the same branch and the same PR is updated. It stops on green, on blocked, or when
  nothing settles within `timeoutSecs` (a pending check is not fed back), and it never merges.

  `/pr run` also picks up commits that exist locally but were never pushed (a direct commit on
  the base branch still stays put — that is a push onto `main`), and it no longer writes a
  `prux: <session title>` commit: the staged summary goes to the current model, which writes a
  subject plus an optional body, falling back to the session title only when no model is selected,
  the request fails or the answer is empty. That request carries the session id
  (`x-opencode-session`; opencode providers answer `400 MissingSessionID` without it) and reuses
  the session's own system prompt, so commit conventions written there still apply. PR titles and
  bodies are written the same way, from the branch diff against its base, except that the PR
  request keeps its **own** system prompt — the session one carries the tool protocol, and a
  one-shot request has no `tools` — and asks for JSON (`{"title", "body"}`), because splitting
  lines turns any stray preamble into half a title. `/pr new` walks the same path but never looks
  for an existing pull request and never reuses the delivery branch: it always creates a fresh
  `prux/<date>-<time>` branch, so the result is a brand-new PR (GitHub does not allow two open PRs
  on one head branch anyway); with a clean tree and nothing unpushed it does nothing.

  Only an open pull request is reused: a closed or merged one is announced and a new PR is opened,
  and the CI poll addresses that PR by number instead of by branch, so the old one cannot shadow
  it. It will not guess a remote (no `origin`, no `gh` login, no detectable default branch all
  stop with a reason). Config lives in `agent_dir()/extensions/pr.json` with no project-level
  override — a file committed into a repository should not be able to change where your work is
  pushed. Commands `/pr status|run|new`. Off by default, `Dev` mode only: it writes git history
  and pushes.

- **Memory entries now need evidence, and a maintenance pass reconciles them**: extraction asks for
  the concrete thing that supports each item — the failing command and its error, the file and
  symbol, or the user's own sentence quoted back — and drops items that answer with an empty
  string. Evidence is stored with the entry and used by the reconcile pass but is never injected
  into the prompt, so the injected block stays compact and stable. Every `maintainEvery`
  successful extractions (default 4) one extra request reconciles the stored entries and must
  return exactly one decision per id: `keep`, `adapt` (rewrite a vague or two-facts-in-one-sentence
  entry, optionally lowering its confidence) or `retire` (no longer true, or fully covered by
  another entry). A missing, duplicate or unknown id, or an `adapt` without content, fails the
  whole batch and writes nothing — half-applied decisions leave the store in a state nobody can
  reason about. `/memory maintain` runs a pass on demand; older stores without the `evidence` field
  still load.

### Changed

- **An isolated subagent starts from the repository's existing build artifacts instead of a cold
  tree**: `Agent({ isolation: "worktree" })` creates a fresh worktree, so the first `cargo build`
  recompiled every dependency. Gitignored artifacts (`target/`, `node_modules/`, …) are now copied
  into the copy as reflinks where the filesystem supports them (btrfs, XFS, APFS): the copy shares
  the original's disk blocks, so it costs milliseconds instead of gigabytes. Only reflink is
  attempted — an unsupported filesystem (ext4, NTFS, tmpfs) or a copy that crosses filesystems is
  skipped entirely rather than falling back to a full copy that would be slower than copying
  nothing. Copied artifacts are ignored files, so they never land in the branch an isolated run
  leaves behind.

- **`/cwd` opens a recent-directory picker, and switching directory always starts a new session**:
  `/cwd` with no argument no longer just prints the current directory — the terminal UI opens a
  searchable list of the directories in `recent-cwds.json` and the desktop GUI opens the same list
  with its `Browse…` entry. `/cwd <path>` resolves `~` and relative paths, rejects a target that is
  not an existing directory without touching the working directory, records the target in
  `recent-cwds.json`, then switches to it **and opens a new session** (the previous session file
  stays in its old directory). Entries whose directory no longer exists are pruned from the list at
  startup, and both UIs read and write the same file.

- **Config panels take bulk and in-place edits**: `Ctrl+A` and `Ctrl+X` enable / disable every
  matching row in the `/skills` and `/extension` panels (terminal and desktop; the terminal
  `/extension` panel already had them), and changing a setting with `Enter` now keeps the list,
  the filter and the highlighted row instead of closing the panel, so several settings can be
  flipped in a row. As with Space, a bulk toggle only edits the unsaved draft: `Ctrl+S` applies and
  writes `settings.json`, and a search term narrows the target to the rows it matches. The stop
  square on the send button is drawn in the accent colour rather than inverted.

### Fixed

- **A new session's file path reaches the UI once it lands on disk**: `/new` starts a deferred
  session, so the switch reply has no path — and nothing ever sent one later. Until the next switch
  `/fork` answered `no current session`, `/resume` did not mark the running session as current, and
  extensions keyed the session as `default`. The worker now reports the path the moment the first
  message writes the file, and the switch is re-dispatched to extensions with it.
- **Left/Right now reach the panel**: the TUI key translation never mapped `←`/`→`, so every
  overlay saw them as "some other key" — the `ask-user-question` tabs only moved on `Tab`, and
  no extension could bind horizontal navigation. Both arrows are mapped now.
- **Transient failures retry, and the retry matcher is no longer fooled by unrelated numbers**:
  `upstream_stream_read_error`, `stream was interrupted`, `Upstream HTTP/2 stream failed`,
  `stream disconnected before completion` and a failed Google Vertex OAuth token refresh
  (`request to … failed, reason:`) all fall through to the retry policy instead of failing the
  turn on the first attempt. A `429` / `5xx` only counts when it stands alone: `req_011CU503xyz`
  and `5242880 bytes` used to match anywhere in the error text and cost a pointless backoff. A
  ChatGPT Business workspace limit is never retried — only an admin can raise it, so backing off
  just wasted turns.

- **Compaction uses the model's output limit, and any HTTP 413 counts as context overflow**:
  compaction and branch summaries no longer derive a cap from `compaction.reserveTokens` (which
  only decides *when* to compact), so a thinking model can no longer exhaust the summary budget
  and fail with "generation hit the token cap". An oversized request body — for example a session
  full of large images on GitHub Copilot — answers 413, which now compacts and retries instead of
  failing.

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
- **A second overlay no longer replaces the first**: when two extensions ask for input at the
  same time, the second one used to take over and the first was simply gone. The covered overlay
  is now kept on a stack and comes back as soon as the one above it closes; switching sessions
  still closes all of them.

## 中文

### 新增

- **新增桌面 GUI：`prux-gui`**：把同一个 agent 搬进窗口——共用 `agent_dir()`、`settings.json`、
  会话文件与扩展，模型链、工具与会话内容与终端完全一致，同一个会话能在两边轮流打开。窗口分三栏：
  左侧会话列表（搜索、按时间分组、改名 / 删除、可收起可拖宽，底部是分支与工作目录），中间对话流
  （Markdown、表格、公式、mermaid、图片、带行号的 diff、工具卡片与耗时、排队区、停靠面板、
  多行输入卡片与一行状态行），右侧 Files / Changes 面板（可收起，diff 带行号、可横竖滚动）。
  斜杠命令、`@` 路径补全、模型与思考级别、技能 / 扩展 / 设置、登录与项目信任、`/rewind`，
  以及扩展发出的提问、覆盖层与独立设置面板都在窗口里可用；扩展命令只写一份，两个宿主共用
  同一张注册表。键位固定成一套（含义与终端一致，不读 `keybindings.json`），列表与覆盖层全键盘
  可达，鼠标可选中复制、中键粘贴、拖动面板改宽；主题、缩放、会话栏开关与自绘标题栏（`gui.frame`）
  收在 `gui` 段下写回 `settings.json`，设置面板的开关（含隐藏思考块）改完立即生效。
  `prux-gui --tui` 用同一个二进制启动终端界面，用法见 `docs/gui.md`。

- **面板补上键位提示**：会话选择器现在写明 `Ctrl+R rename · Ctrl+D delete`
  （此前只有「Enter confirm」），登出列表也在清 key 前说明 Enter 的作用。

- **`prux acp` 把 prux 变成编辑器的 agent 后端**：新增 `acp` 扩展注入 `acp` 子命令——`bootstrap()`
  把**已装配好的** Agent 交给它，因此编辑器拿到的是与 TUI 完全相同的模型链、工具表、会话文件与
  扩展钩子，而不是第二套 agent loop。它在 stdin/stdout 上讲 NDJSON + JSON-RPC 2.0，应答
  `initialize` / `authenticate` / `session/new` / `session/prompt` / `session/cancel`，把文本、
  思考、工具调用起止以 `session/update` 通知流式推给客户端；回合执行期间同时监听 stdin，取消
  即时生效，不会排在一个慢回合后面。客户端在 `initialize` 里广告
  `clientCapabilities.elicitation.form` 时，`ask_user_question` 的问卷送成 `elicitation/create`
  表单由客户端渲染；没广告的客户端（Zed 1.3.6 根本不发 `elicitation` 能力）退化为每道单选题发一次
  `session/request_permission`，多选题按未作答报告，其余会阻塞等作答的选择面板（如权限门控）
  一律走 `session/request_permission`。单会话（`session/new` 返回 `bootstrap()` 已打开的会话），
  不支持 `session/load`，prompt 只取文本块；该扩展不注册工具、不订阅钩子——不开 `prux acp`
  时它什么都不做。默认不启用，且只在 `Dev` / `Creator` / `All` 模式下可用：在 `/extension`
  面板（或 `settings.json`）开启后才有 `prux acp`，`Minimal` 模式下子命令完全不存在。

- **`prux --help` 现在会列出扩展注入的子命令**：`prux acp` 与 `prux mcp` 都成为真正的 clap 子命令，
  由拥有它们的扩展注入，于是它们带着一行说明出现在 Commands 段——且只在该扩展启用时才出现。
  `mcp` 的子命令从内置 CLI 移进了 `mcp` 扩展（该扩展默认关闭），因此 `prux mcp ...` 现在需要先
  启用它；`prux acp --help` 打印自己的用法。

- **`vet` 只跑改动真正涉及的那几条检查**：`diffScoped` 缺省开启，结算前先读 `git status
  --porcelain`，只保留与改动语言匹配的检查——只改 Rust 就跑 `cargo …` 那几条，只改文档就一条
  都不跑，改到依赖清单或 lock 文件则全跑。认不出语言的命令（`just check`、`cargo test && pnpm
  test`）永远跑——跳过它们等于假的通过；不是 git 仓库时退回全量。`/vet run` 手动路径仍然跑全部。


- **新增 `hooks` 扩展：在生命周期时机跑你自己配的命令**：四个事件——`PreToolUse`、
  `PostToolUse`、`UserPromptSubmit`、`Stop`——由 agent 目录下的 `extensions/hooks.json` 驱动，
  格式与 Claude Code 同形，已有配置可以直接搬。每条钩子的 stdin 是一行 JSON（`hook_event_name`、
  `session_id`、`cwd`，以及 `tool_name` / `tool_input` / `tool_response` / `prompt` /
  `stop_hook_active`），结果看退出码加 stdout 上的可选 JSON：退出码 `2` 按阻止处理、stderr 当
  理由；`0` 读 `decision: block` / `approve`、`hookSpecificOutput.permissionDecision`
  （`allow` / `ask` / `deny`）、`additionalContext` 与 `updatedInput`。于是钩子可以拒绝一次调用、
  弹确认面板、改写入参、覆盖模型看到的工具结果、拒绝一次提交，或者在 agent 想收工时要求继续
  （`Stop` 最多连续续跑 5 次）。多个钩子的意向按最严合并，任何一条 `deny` 就让本批停下；
  `matcher` 是大小写不敏感的工具名正则；每条命令自带超时（缺省 10 秒），超时连子进程一起杀掉。
  写坏的钩子只提示一次，绝不挡住调用。`/hooks` 打印生效的钩子与配置错误，`/hooks settings`
  打印同一份清单，`/hooks reload` 丢弃缓存后重读文件（手改配置本来就会按修改时间 / 文件长度
  变化自动生效，缓存只是免掉重复读盘）。默认不启用（`Dev` / `Creator`）。

- **新增 `rtk` 扩展：用外部 `rtk` CLI 压缩 bash 命令**：每次 `bash` 调用前先跑
  `rtk rewrite <命令>`，rtk 给出紧凑等价写法时就用改写后的命令执行（退出码 `0` 或 `3` 且有
  输出；`1` 表示没有可用的改写），从而让工具输出、以及它吃掉的上下文小下来。**全程
  fail-open**：二进制缺失、超时（5 秒）、输出为空、退出码不认，都按原命令执行。它唯一不会
  沉默的是「启用了但没装」——`PATH` 上找不到 `rtk` 会报一次错并给出下载地址
  （https://github.com/rtk-ai/rtk）。改写发生在权限判定之前，因此 `permissions` 与沙箱看到的是
  真正要执行的那条命令。默认不启用（`Dev` / `Creator`）。

- **新增 `rollback` 扩展：把某一回合改过的文件退回去**：`edit` / `write` 写盘之前把目标文件快照
  下来，按**回合**分组（一条用户消息 = 一回合）。`/rollback` 列出改过文件的回合，
  `/rollback diff <n>` 看某回合动了哪些文件，`/rollback <n>` 把那一回合改过的文件退回回合开始前
  的样子。两个细节是有意的：回合里**新建**的文件还原等于删掉，而不是留一个空文件；覆盖之前会先给
  当前内容拍一张快照，所以手滑的还原本身也能再还原回来。快照落在
  `agent_dir()/checkpoints/<workspace-key>/`（按工作目录分桶，满 500 条清掉最老的 20%）；回合
  进行中 `/rollback` 一律拒绝。它**不**并进 `rewind`：那个只退会话分支，而「只想撤回一句误发的话」
  不该顺带为每次写盘买一份文件复制。用 git 的项目通常 `git restore` 就够了，本扩展补的是 git
  退不了的那部分——本回合开始时尚未提交的内容。默认不启用（`Dev` / `Creator`）。

- **新增 `worktree` 扩展：把当前会话搬进 git worktree 副本里干活**：`/worktree enter [name]` 在
  `agent_dir()/extensions/worktrees/<仓库名>-<名字>` 建一份 `HEAD` 快照的副本并把会话的工作目录切进去
  （调用方在仓库子目录时，副本里保留同样的层级）；`/worktree exit` 把副本里的改动提交成
  `prux-wt-<名字>` 分支（同名已存在就加时间戳后缀）后删副本、切回原目录；`/worktree drop` 直接
  删副本丢弃改动；裸 `/worktree` 报告当前状态。三条口径是有意的：工作目录里**未提交的改动不会
  跟着进副本**（它们留在原仓库，进入时会提示），`git commit` 失败时**副本原样保留**并报错而不是
  把活删掉，回合进行中一律拒绝切换。副本内的提交带 `--no-verify`——用户的钩子不在临时副本里跑；
  副本删不掉时退一步 `git worktree prune` 清掉注册项。只提供命令、不提供工具：换目录是用户的选择，
  而工具执行期拿不到会话 `cwd` 的写能力。默认不启用（`Dev` / `Creator`）。

- **`permissions` 也管网络主机了**：新增第五层 `domain`，按一次调用会连的每个 `http(s)://` URL
  的主机判定——bash 跑的每条语句（含 `$(…)` 里的）以及其它工具入参里任意深度的 URL，因此把 URL
  藏在 `arguments.url` 的 MCP 调用也拦得住。主机按小写匹配，可写 `host:port`（如 `localhost:8080`），
  IPv6 带方括号。`{"*": "allow", "*.internal": "deny"}` 就够把 agent 关在内网里；不写这一层则完全
  不改变现有行为，出站照旧不设限。`ask` 走与其它层同一个确认面板，`/permissions allow domain
  "*.evil"` 可为本会话放行一条模式。

- **新增 `papers` 扩展：文献检索与全文获取**：`web_search` 拿得到网页，拿不到「哪些论文、
  哪篇原文」。四个零鉴权源并发查询——alphaXiv（社区票数，只覆盖 arXiv）、OpenAlex（引用数、
  DOI、跨库）、bioRxiv（OpenAlex 按来源收窄）、PubMed（两步：先按相关度排名，再 `efetch`）——
  按 arXiv id / DOI / OpenAlex id / PMID 有交集即合并，票数与引用数取较大值。`paper_search`
  接受 `query` / `sources` / `num_results` / `published_after` / `published_before` 与 alphaXiv 的
  `prioritize`（`default` / `recency` / `popular` / `historical`）；单个源失败只追加一行提示，
  其余源结果照常返回。`paper_get` 把裸 id 或带前缀的 arXiv id / DOI / OpenAlex id / PMID 路由到
  对应的元数据源，再走正文回退链：alphaXiv Markdown 全文 → 它的生成式综述 → arXiv PDF
  （`export.arxiv.org`）→ OpenAlex 给的开放获取 PDF。PDF 只抽文本层，扫描件会明确说「没有
  文本层」而不是返回空内容。正文按 20000 字符截断，并在末尾给出下一次调用用的 `offset`。
  两个工具都是 `tool-search` 后面的延迟工具；默认不启用（`Dev` / `Creator`）。

- **新增 `injection-guard` 扩展：工具结果像提示注入时先提示一声**：agent 整天在往自己上下文里灌
  不可信文本——抓回的网页、解析出的 PDF、远端读来的文件、`curl` 的输出——里面一句「忽略上面的
  指令」，正是模型最容易照做的形态。现在每条工具结果都会过一遍模式表打分：指令覆盖、套系统提示、
  角色劫持、聊天模板标记（`<|im_start|>` / `[INST]` / `<<SYS>>`）、行首角色标记、
  `new instructions:`、`do not tell the user`（写法放宽到 `don't` / `never`，宾语也包括 `them` /
  `anyone`；这些拼法过去只算作一条更弱的规则）、外传数据、藏在 HTML 注释里的指令。变形先还原再
  打分：去掉零宽 / 双向 / 标签区字符，并把 base64 与十六进制载荷解出来（只认解出来像文本的，段数
  与单段长度都有上限），所以 `ig\u{200B}nore` 或整句 base64 一样能命中。同时补上两条外传诱导规则：
  URL 查询参数里带凭据形状的值（`?token=…` / `?api_key=…`，markdown 图片同样算——渲染即发请求，
  不用点），以及「读某个敏感路径 + 发到某处」的指令句，要求读动词、敏感路径、传输动词落在同一句
  里（`.ssh/`、`id_rsa`、`authorized_keys`、`.aws/credentials`、`.npmrc`、`.netrc`、
  `.git-credentials`、`.kube/config`、`.docker/config.json`、`/etc/shadow`）。总分 ≥3 就在
  聊天区提示一次（不落库、不进模型上下文）。结果本身**绝不改写**——打码会破坏 diff 与代码块，
  而且模型要看原文才能自己判断。同一条结果只提示一次。默认不启用（`Dev` / `Creator`）。

- **新增 `hardware` 扩展：本地芯片手册、引脚表与真板子**：把转成 Markdown 的手册（和引脚 CSV）
  放进 `agent_dir()/extensions/hardware/datasheets/`，可以按芯片建子目录；`hardware_search` 用与
  其它检索同一套词法切词给每个块打分，返回 `file:line` + 片段正文。命中在路径里的块额外加权，
  所以搜 `usart` 会优先出 `stm32f4/…` 而不是无关文件；`path` 收窄前缀、`limit` 限条数，资料目录
  每次调用重扫（不建索引缓存）。引脚别名不需要第二套机制——引脚表的一行
  （`PA9,USART1_TX,TIM1_CH2`）就是一块，引脚名和复用功能名都能搜到。

  同一扩展也直连板子。探针通道走 `probe-rs`——`hardware_board_info` 读芯片名与架构、
  `hardware_memory_map` 列 RAM / Flash 区间、`hardware_memory_read` 把任意地址段渲染成 hex dump；
  没有探针时前两个回落到内置板卡表（`nucleo-f401re` / `nucleo-f411re` / `arduino-uno` /
  `arduino-mega` / `arduino-uno-q` / `esp32` / `rpi-gpio`），所以「这块板是什么芯片」不插板也能答。
  引脚走 `gpio_read` / `gpio_write` / `hardware_capabilities`：对端是串口上的行分隔 JSON 桥接固件，
  或本机 `127.0.0.1:9999` 的 Uno Q Bridge；`gpio_rpi_*` 与 `rpi_system_info` 直接操作树莓派的 BCM
  引脚（`rppal`，仅 Linux）。`hardware_flash` 经探针烧 `.elf` / `.hex` / `.uf2` / `.bin`，
  `pico_flash` 把 UF2 拷进 Pico 的 bootloader 挂载点——两者都是破坏性操作，启用 `permissions`
  时受其审批约束。`arduino_upload` 调 `arduino-cli`，`device_read_code` / `device_write_code` /
  `device_exec` 调 `mpremote` 跑 MicroPython 板子。串口路径走白名单（`/dev/ttyACM*` /
  `/dev/ttyUSB*` / `/dev/tty.usbmodem*` / `COM*`），每次调用重新打开端口。配置在
  `agent_dir()/extensions/hardware/config.json`（`boards`、`chip`、`devices`、`baud`）。工具全部是
  延迟工具（需 `tool-search`）；`/hardware` 打印资料目录、板卡配置与工具列表。默认不启用
  （仅 `Dev` 模式可用）。

- **新增 `leak-guard` 扩展：凭据外发前先拦一道**：`permissions` 管的是「哪个工具能跑」，不管
  「什么内容出去」——`curl -H "Authorization: Bearer …"`、或者把 `.env` 贴进 prompt，以前都是
  原样发走的。`leak-guard` 两头都扫：每次请求前扫每条消息的文本块，每次工具调用前扫入参。规则表
  覆盖 OpenAI / Anthropic / GitHub / GitLab / AWS / Slack / Google / Stripe 的 key、JWT 形状的
  token、私钥块，以及任何「长度 ≥32、大小写与数字齐全、Shannon 熵 ≥4 bits/字符」的串（纯 hex
  跳过，所以 commit 与内容哈希不会误报）。路径不会连成一个候选串，机器生成的标识（UUID、哈希、
  `2026-11-02T10-25-54-666Z_<uuid>` 这类会话名）直接跳过，告警记录满了改为挤掉最早一条而不是
  整体清空——历史里这类名字一多，记录每轮都被清空，同一个提示就会每回合重弹一次。每条提示附上
  命中片段的首尾预览（如 `high-entropy: q8Zr2NvT…Zx4Q`）；被拒绝的工具调用只报类别，因为那条
  原因会回到模型上下文。档位 `warn`（默认）只提示；`block` 只把**本次请求**的命中替换成
  `[REDACTED:<类别>]`，并拒绝携带凭据的工具调用、把命中的类别回给模型。思考块只扫不改——它的
  文本带签名，改了签名就失效。同一 `(类别, 片段)` 只提示一次。档位存在
  `agent_dir()/extensions/leak-guard.json`——`/leak-guard warn|block|off` 设档，裸 `/leak-guard`
  打开扩展设置面板。默认不启用（`Dev` / `Creator`）。

- **新增 `processes` 扩展：后台进程监督**：内置 `bash` 工具只有「前台执行 + 超时杀进程」，
  dev server、watcher 这类长命命令会把一整轮卡到超时。`background_bash` 把命令丢到后台，立刻
  返回 `job-N`、pid 与日志路径——stdin 接 `/dev/null`，stdout/stderr 写进那个日志文件而不是
  继承终端。`read_process_output` 返回输出**快照**（状态、pid、耗时、最后 N 行；读不消费日志，
  省略 `id` 列出全部 job），`stop_process` 杀掉整个进程组。`/ps` 三段式切换停靠段显隐
  （同 `/todos`），`/ps list` 打印清单，`/ps stop <id>` 停止、`/ps clear` 丢掉已完成记录。停靠段只列
  运行中的 job——刚结束的行在下一帧被刷掉，全部结束时段落与面板自动收起。job 不随回合结束消失，
  禁用本扩展会停掉全部运行中的 job。默认不启用（`Dev` 模式）。

- **新增小球落位式启动横幅**：新的 `bouncyballs-banner` 扩展把左侧 logo 拆成一颗颗球——art 的
  每个非空白字符都从上方按 out-bounce 缓动落下、落位时显形为该格原本的字符，最后定格成与静态版
  完全相同的 art。右侧信息区、竖线与宽高三档规则与普通 banner 共用，普通 banner 仍可在
  `/extension` 面板切回（两者互斥，默认生效的是动画版）。banner 扩展新增了「请求重绘」钩子，
  动画因此跑在 TUI 的 80ms tick 上，不会停在半空的一帧。

- **新增 `ask-user-question` 扩展：让模型问，而不是猜**：只加一个工具 `ask_user_question`，
  弹出最多四题的覆盖层面板，每题 2–4 个写好的选项——单选、多选、每题末尾自动带一行
  `Type something.` 自由作答、`n` 写逐题备注、提交页写整份问卷的全局备注。作答在同一轮
  以 `details.answers[]` 回到模型（`kind` = `option` / `custom` / `multi`，附带 `selected` /
  `notes` / `preview`），另带 `globalNote` 与 `unanswered`，因此「还没定」和「已经定了」能被
  分开，模型不必重问。保留标签与上限在弹面板之前就校验：1–4 题、每题 2–4 个选项、`header`
  最长 16 字符、`label` 最长 60 字符，`Other` / `Type something.` / `Next` 作为标签一律拒绝。
  提交按钮写 `Submit`（原先的 `Submit answers` 在窄面板里会跟说明文字抢宽度），与 `Cancel`
  等长。没有 UI（非交互运行）时工具立刻返回 `no_ui` 而不是干等；同一时刻只允许一份问卷，第二份
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

  另有五个子命令形式的检查清单：`/ponytail review [目标]`（改动审查：bug、安全、真实负载、
  缺测试、性能、可删的代码——每条都说明这段代码在做什么、哪里会出错、怎么修、不修会怎样）、
  `/ponytail audit [目标]`（同一套审查跑全仓，按严重度排序）、`/ponytail debt [标记词]`
  （把 `shortcut:` 注释收成债务账本）、`/ponytail gain`（已发布的 benchmark 记分牌）——这些正文
  随二进制内嵌、作为 `<skill>` 块投递给模型，不落盘，扩展关闭时也不会有技能描述漏进系统提示，
  回合运行中投递会作为 steer 插进当前回合。而 `/ponytail help` 的速查卡直接在终端里渲染：不花
  一个回合，也不进会话历史。默认不启用，仅在 `Dev` 模式下可用：它改写的是模型每一轮的写作风格，
  必须由用户显式开启。

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

- **`computer_crop` 放大已有的图**：`computer` 扩展新增第 15 个工具，把磁盘上的图片文件按
  `[x, y, width, height]` 裁出来返回——想看清截图里的某一块不必重新截屏。像素取自落盘的原图，
  所以即便交付给模型的那张图被缩过，文字依旧清晰；矩形超出图片范围会连真实尺寸一起报错并拒绝，
  回执里给出偏移量，裁出来的点可以换算回桌面坐标。

- **Rex 终端被认出来了**：`TERM_PROGRAM=rex` 下内联图片走 kitty 协议（不再退化成半块字符），
  配色按 truecolor 处理，超链接与图片都能正常显示，而不是退成 `text (url)`。

- **`/changelog` 按 Markdown 渲染，信息覆盖层的滚动条可以拖动**：更新日志现在像助手回复一样
  排版——标题、列表、行内代码与代码块都有样式，不再是一整块纯文本；`/changelog`、`/hotkeys`
  以及列表覆盖层的详情页右侧那根细条能抓来拖，命中区比画出来的 4px 宽，指针离开轨道也继续
  跟着走。`↑` / `↓` / `PgUp` / `PgDn` / `Home` / `End` 同样能翻这些面板。

- **`browser` 扩展补上标签页、历史、悬停、页面状态、控制台与 JS 对话框**：`back` / `forward` /
  `reload` 走标签页自己的导航历史，不必再手写一次导航；`close_tab` 按索引或目标 id 关标签页。
  `hover` 只移动指针不点击（下拉菜单与 tooltip 需要），`dblclick` 发两轮点击，`inspect` 读元素的
  单项状态——`value` / `text` / `html` / `attr` / `count` / `visible` / `enabled` / `checked`——
  不用再从快照文本里猜。`snapshot` 支持 `scope` 限定子树与 `depth` 限制层数，`wait` 支持 URL
  片段与任意 JS 条件，`press` 支持 `Control+a` / `Meta+v` 这类组合键。attach 一个标签页时顺带打开
  `Runtime` / `Log` / `Page` 的事件上报，按标签页缓冲（200 条）：新的 `console` 动作取走并回放
  缓冲——`console.*` 输出、未捕获异常、资源加载失败；新的 `dialog` 动作报出打开中的 `alert` /
  `confirm` / `prompt` 并接受或拒掉它，对话框卡住页面时命令超时的报错会点名这一点，不再看起来像
  连接已经死掉。`click` / `dblclick` 先检查元素中心点，被别的元素盖住时直接报错并点名盖住它的
  元素（如 `covered by <div#banner>`），元素 `disabled` 或没有可点面积时同样报错。

- **`/session` 有了子命令，其中三个用于删除会话**：`/session statistics` 就是原来不带参数的
  `/session` 打印的会话概要；`/session rm-current`、`/session rm-project`、`/session rm-all`
  分别删除当前会话、当前项目目录下的所有会话、所有项目目录下的所有会话。三者都先弹 Yes/No 确认框，
  写明会删掉多少个会话文件、在哪个目录，删完回报成功条数。`rm-current` 删完自动开新会话——否则
  下一次写入会把刚删掉的文件重新建出来。删掉的会话先进回收站而不是直接 unlink；每个工程的会话
  目录也跟着最后一个会话一起走（`/new` 与侧栏的 `+` 不再预先建目录），目录列表因此与实际内容
  一致。`/session <序号>` 的按序号切会话保持不变，子命令名在输入框里可补全。

- **新增 `sandbox` 扩展，把 shell 命令关进 OS 级边界**：`permissions` 是执行前的判定，动态拼出来
  的命令只能靠猜；这个扩展把文件系统规则交给内核。启用时探测后端（Linux `bwrap`、macOS
  `sandbox-exec`）并**真的用它起一个 `true` 进程**——bubblewrap 在禁用用户命名空间的容器里装得下
  却起不来，只有到 spawn 才暴露。此后每次 `bash` 调用都在「整棵文件系统只读 + 当前工作目录与系统
  临时目录可写 + 网络放行」下执行：`~/.ssh` 够不到了，`~/.cargo` 也一起变成不可写，要让它编译就把
  缓存目录指到工作目录里。探测失败**不会**退回无沙箱执行，而是装上一个「一律拒绝启动」的包装器并
  附上原因，失败因此是看得见的；要恢复无沙箱执行，显式在 `/extension` 里关掉本扩展。`/sandbox`
  打印当前后端、可写路径与网络状态。默认不启用。

- **`/debug` 回答会话到底做了什么，也会点名 prux 不认识的设置键**：`/debug receipts` 逐行读会话
  里本就留下的记录——每次调用的 `tool_started`、给退出码与 `edit` 精确增删行数（来自 patch）的
  `toolResult` 消息、回合级失败的 `operation_finished`——再读 `permissions` 的 review log 补上
  **根本没跑起来**的那些调用（`deny` / `ask`），后者在会话里不留痕迹。回执先给汇总（改了哪些
  文件、跑了多少命令、MCP 调用、子代理、失败、审批），再按时间序列出本轮经过；
  `/debug receipts <n>` 抬高明细行上限。解析是逐行的：命令 busy-safe，正在跑的这一回合写了一半的
  尾行被跳过，而不是让整份回执失败。`/debug settings-key` 列出每个未知的顶层设置键与它的
  `文件:行号`——能解析、但键名打错的 `settings.json`（`"compactReserveToken"`）原本被静默忽略，
  那个值从来没生效过——有已知键的编辑距离在 2 以内时给出 `did you mean "…"?`，没有问题时只回一行
  确认。只存在于全局文件的键（`theme`、`skills` 等）写在 `.prux/settings.json` 里同样会被点名
  ——项目层只认 `defaultTools` / `disabledExtensions` / `enabledExtensions`。语法错误仍归启动警告；
  每次执行都重读这两个文件。

- **新增 `pr` 扩展：`/pr run` 与 `/pr new` 把改动交付成一个 PR**：`vet` 只证明本机检查过了，
  `pr` 补齐交付那一串机械动作——建分支、提交、推、开 PR、等 CI 与评审，然后把结论贴回聊天区。
  **没有结算边界钩子，也不把失败喂回模型**：没人替你决定要不要推，坏掉的一轮不会自己变成 PR。
  失败的检查（`名称 — 结论`）与 `CHANGES_REQUESTED` 的评审意见连同 PR 链接一起列出，修完再跑一次
  `/pr run`——改动落在同一条分支上，同一个 PR 被更新。停止条件：全绿 / 有阻塞 / 等满
  `timeoutSecs` 仍无结论（pending 不猜结果），且**永不 merge**。

  `/pr run` 也接手已提交但没推的提交（直接提交在基线分支上的仍不动，那等于往 `main` 上推），
  并且不再往历史里塞一条 `prux: <会话标题>`（标题是用户的第一句话，常常和这批改动对不上），而是
  把暂存的改动摘要交给当前模型写一条 subject + 可选 body；没选模型、请求失败、输出为空时才回落
  会话标题——写不出信息不该让交付停在本地。这次请求带上当前会话 id（`x-opencode-session`：缺了它
  opencode 系 provider 会直接回 `400 MissingSessionID`），系统提示词就用**当前会话那一份**，
  用户写在里面的提交约定照生效。PR 的标题与正文同理，从整条分支相对基线的改动摘要写出，只是那份额外
  用**自己的**系统提示词（会话那份带着工具协议，而一次性请求没有 `tools`，模型会把工具调用块当
  普通文本吐出来）并要求 **JSON 输出**（`{"title", "body"}`：按行切分一旦多写一句前言就会把半句话
  当成标题）。`/pr new` 走同一条交付路，但不查已有 PR、也不续用当前交付分支——总是新建一条
  `prux/<日期>-<时分>` 分支再推，因此拿到的是全新的 PR（GitHub 也不允许同一个 head 分支挂两条
  开着的 PR）；工作区干净且没有未推送提交时它什么也不做。

  只复用开着的 PR：已关闭 / 已合并的那条在聊天区说一声后另开一个新的，等 CI 也按 PR 号读状态，
  不会被旧的那条挡住。当前就在基线分支时先建 `prux/<日期>-<时分>` 分支再干活，已在 `prux/`
  分支上则续在同一条；工作区干净且不在 `prux/` 分支上什么也不做。不猜远端（没 `origin`、
  `gh` 没登录、探测不出默认分支都停下报原因）。配置在 `agent_dir()/extensions/pr.json`，
  **没有项目级覆盖**——能提交进仓库的配置不该有权改你的推送目标。命令
  `/pr status|run|new`。默认不启用，仅 `Dev` 模式：它会写 git 历史并推远端。

- **记忆条目现在需要证据，并有维护轮做 reconcile**：抽取时要求每条附上支撑它的具体证据——失败的
  命令与报错、文件与符号，或用户自己的原话——答成空串的条目直接丢弃。证据随条目存档并供维护轮
  判定，但**不进注入块**，注入内容因此仍然紧凑且稳定。每成功抽取 `maintainEvery`（缺省 4）次，
  额外一次请求把已存条目 reconcile 一遍，要求每个 id 恰好一个决定：`keep`、`adapt`（改写含糊或
  把两件事挤在一句里的条目，可下调置信度）、`retire`（不再成立，或已被另一条完全覆盖）。缺 id、
  重复 id、未知 id、`adapt` 没给正文——任何一条不满足就整批失败、一个字节都不改（半套决定会让
  记忆图处在谁也说不清的状态）。`/memory maintain` 可按需触发一轮；旧存储没有 `evidence` 字段
  照样能读。

### 变更

- **隔离子代理不再从冷目录起步，而是直接复用仓库里已有的编译产物**：`Agent({ isolation: "worktree" })`
  建出的是一份干净的 worktree，于是第一次 `cargo build` 要把所有依赖重编一遍。现在会把主仓库里被
  gitignore 的产物（`target/`、`node_modules/` …）以 reflink 方式复制进副本——文件系统支持时
  （btrfs、XFS、APFS）副本与原目录共享磁盘块，代价是毫秒级而不是几个 GB。只尝试 reflink：
  不支持的文件系统（ext4、NTFS、tmpfs）或跨文件系统的复制会被整体跳过，而不是退化成一次比不拷
  更慢的全量拷贝。复制过来的都是被忽略的文件，因此不会混进隔离运行留下的那个分支。

- **`/cwd` 改成打开最近目录选择面板，且换目录一律新开会话**：不带参数时不再只打印当前目录——
  终端 UI 打开一个可输入过滤的最近目录列表，桌面 GUI 打开同一份清单（带 `Browse…` 入口）。
  `/cwd <路径>` 支持 `~` 与相对路径，目标不是已存在目录就报错且不动工作目录，存在则记入
  `recent-cwds.json` 再切过去并**新开一个会话**（原会话文件留在原目录）。清单里已经不存在的目录
  会在启动时清掉，两种界面共用同一份文件。

- **配置面板支持批量与就地修改**：`/skills` 与 `/extension` 面板（终端与桌面）新增
  `Ctrl+A` 全选 / `Ctrl+X` 取消全选，终端 `/extension` 面板本来就有；设置面板按 `Enter` 改值后
  不再关面板，过滤词与高亮行一起留着，可以连着改好几项。与空格一样只改未保存的草稿：`Ctrl+S`
  才应用并写回 `settings.json`；搜索框有词时只作用于命中的行。发送按钮上的停止方块改用品牌
  强调色，而不再取反色。

## 修复

- **新会话落盘后会把文件路径补发给 UI**：`/new` 起的是延迟落盘会话，切换回执里没有路径，
  而之后也没有任何地方补发——于是在下一次切会话之前，`/fork` 一律回 `no current session`，
  `/resume` 不把当前会话标成当前，扩展也把会话记到 `default` 键上。现在首条消息写出会话文件时
  worker 就把路径报给 UI，并带着它重发一次会话切换给扩展。
- **`←` / `→` 现在能送到面板**：TUI 的按键翻译从来没映射左右方向键，所有覆盖层都把这两个键当成
  「其它键」——`ask-user-question` 的标签页只有 `Tab` 能翻，扩展也无法绑定横向导航。现在两个方向键
  都会映射过去。
- **瞬时故障会重试，重试匹配也不会再被无关数字骗到**：`upstream_stream_read_error`、
  `stream was interrupted`、`Upstream HTTP/2 stream failed`、`stream disconnected before
  completion`，以及 Google Vertex 的 OAuth token 刷新失败（`request to … failed, reason:`）现在都
  走重试策略，不再第一下就判死。`429` / `5xx` 只有独立成 token 才算状态码：`req_011CU503xyz`
  这类请求 id、`5242880 bytes` 这类字节数原来在错误文案里任意位置都命中，会触发无谓的退避重试。
  ChatGPT Business 的工作区上限永不重试——那种额度只有管理员能提，退避只是浪费回合。

- **压缩改用模型的输出上限，任何 HTTP 413 都算上下文溢出**：压缩与分支摘要不再从
  `compaction.reserveTokens` 推一个输出额度（那个值只决定**何时**开始压缩），思考型模型不会再
  把摘要额度吃光后报 "generation hit the token cap"。请求体过大（例如 Copilot 上塞满大图的会话）
  返回的 413 现在先压缩再重试，而不是直接失败。

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
- **第二个覆盖层不再顶掉第一个**：两个扩展同时要输入时，第二个会把第一个挤掉、前者直接消失；
  现在被盖住的覆盖层挂在栈上，上面的关掉就露回来，切换会话仍然会一次关掉全部。
