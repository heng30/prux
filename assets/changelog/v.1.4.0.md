## English

### Added

- **`prux acp` serves the Agent Client Protocol so an editor can drive the same agent**: the new
  `acp` extension injects an `acp` subcommand — `bootstrap()` hands it the fully assembled agent,
  so the editor gets the same model chain, tool set, session file and extension hooks as the TUI,
  not a second agent loop. Over NDJSON + JSON-RPC 2.0 on stdin/stdout it answers `initialize`,
  `authenticate`, `session/new`, `session/prompt` and `session/cancel`, streaming text, thinking
  and tool-call updates as `session/update` notifications. A prompt keeps listening on stdin while
  it runs, so a cancel lands immediately instead of queueing behind a slow turn. Single session
  (`session/new` returns the session `bootstrap()` already opened), no `session/load`, text
  prompts only. The extension registers no tools and no hooks — without `prux acp` it does
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


- **Every code block in the desktop GUI has a copy button**: the block's header row shows a
  copy icon on the right; one click puts the block's source on the clipboard and raises a
  `Copied to clipboard` toast.

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
  `limit` caps the result. Pin aliases need no second mechanism — one CSVs line
  (`PA9,USART1_TX,TIM1_CH2`) is a chunk, searchable by either name. The directory is rescanned per
  call (no index cache).
- **The `hardware` extension also drives boards over a probe, serial and GPIO**: `probe-rs` gives
  `hardware_board_info` (chip and architecture), `hardware_memory_map` (RAM / Flash ranges) and
  `hardware_memory_read` (a hex dump of any address range); without a probe the first two fall
  back to a built-in board table (`nucleo-f401re`, `nucleo-f411re`, `arduino-uno`, `arduino-mega`,
  `arduino-uno-q`, `esp32`, `rpi-gpio`), so "what chip is this board" needs no hardware. Pins go
  through `gpio_read` / `gpio_write` / `hardware_capabilities`: either a line-delimited JSON
  bridge on the serial port, or the Uno Q Bridge on `127.0.0.1:9999`; `gpio_rpi_*` and
  `rpi_system_info` hit a Raspberry Pi's BCM pins directly (`rppal`, Linux only). `hardware_flash`
  writes `.elf` / `.hex` / `.uf2` / `.bin` over the probe and `pico_flash` copies a UF2 onto a
  Pico's bootloader volume — both destructive, therefore gated by `permissions` when it is
  enabled. `arduino_upload` shells out to `arduino-cli`, and `device_read_code` /
  `device_write_code` / `device_exec` go through `mpremote` for MicroPython boards. Serial paths
  are whitelisted (`/dev/ttyACM*`, `/dev/ttyUSB*`, `/dev/tty.usbmodem*`, `COM*`) and every call
  reopens the port. Config lives in `agent_dir()/extensions/hardware/config.json` (`boards`,
  `chip`, `devices`, `baud`). All tools are deferred behind `tool-search`; `/hardware` prints the
  directory, board config and tool list. Off by default, `Dev` mode only.


- **A `leak-guard` extension flags credentials before they leave**: the `permissions` extension
  gates *which* tools may run, not *what* leaves — a `curl -H "Authorization: Bearer …"` or a
  `.env` pasted into a prompt went out untouched. `leak-guard` scans both sides: every message's
  text block before each request, and every tool call's arguments. The rule table covers
  OpenAI / Anthropic / GitHub / GitLab / AWS / Slack / Google / Stripe keys, JWT-shaped tokens,
  private key blocks, and any run of 32+ characters that mixes case and digits with Shannon
  entropy of at least 4 bits/char (pure hex is skipped, so commits and content hashes stay
  quiet). Mode `warn` (the default) only notifies; `block` replaces prompt hits with
  `[REDACTED:<category>]` in that one request and refuses a tool call that carries one, telling
  the model which category tripped it. Thinking blocks are scanned but never rewritten — their
  text is signature-bound. A repeated `(category, snippet)` pair notifies once. Mode lives in
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

- **The desktop GUI's session sidebar shows how many sessions each group holds**: the `Today` /
  `This week` / `Older` heading now carries its entry count, right-aligned in the caption colour.
  The count follows the search filter, so a filtered list reports what it actually lists.

- **The desktop GUI's `/session` panel can copy the session file path**: the `Session file` value is
  clickable (link colour, underline on hover) and writes the path to the system clipboard with a
  toast confirmation; an in-memory session has no path and stays plain text.

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

- **The desktop GUI switches models and thinking levels from the keyboard**: `Ctrl+P` /
  `Ctrl+Shift+P` step through the model scope (`--models` / `settings.enabledModels`, falling back
  to every configured model only when the scope is empty, keeping the scope's order and skipping
  entries the catalog no longer has); a scoped entry that carries a thinking level sets that level
  before switching. `Shift+Tab` cycles the current model's thinking levels. Both follow the TUI
  bindings.

- **The desktop GUI queues what you send while a turn runs**: `Enter` puts the text into the
  steering queue, so the model reads it inside the same run (before its next step) instead of a
  fresh turn after the fact, and `Alt+Enter` queues a follow-up that is sent once the turn settles.
  Both queues drain by `steeringMode` / `followUpMode` as they do in the terminal, and the waiting
  messages are listed above the input box as `Steering:` / `Follow-up:` — they enter the
  transcript only when actually sent. `Alt+Up` pulls them all back into the input box (steers
  first, then follow-ups, separated by blank lines and kept ahead of the current draft) and empties
  both queues.

- **The desktop GUI's session search answers to a shortcut**: `Ctrl+Shift+H` focuses the search box
  above the left session list (expanding the sidebar if it was collapsed, and taking focus from the
  right panel's search), while `Esc` / `Enter` leave focus and keep the query.

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
  leaves `Up` / `Down` under your control. The command palette lists the toggle as `Sidebar`.

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

- **The desktop GUI's right panel scrolls both ways**: the diff and the file body each keep their
  own scroll position, with a bar on the right edge and one along the bottom; dragging anywhere on
  a bar's track moves the content, and the pointer may leave the track mid-drag. Long lines no
  longer wrap, so the horizontal bar has something to scroll, and opening another file or change
  starts that area at the top instead of inheriting the previous offsets.

- **The desktop GUI's status bar reports the run, not just the message count**: the left side shows
  messages, the share of the context window in use and the cache hit rate; the right side shows
  input / output tokens, cost, output rate in tokens per second, and whether a goal or plan is
  active, how many turns have been sent and how long the round has taken. While the agent works,
  a blinking dot and `Working… (Esc to abort, N queued)` lead the row, and a half-second redraw
  keeps the elapsed time and the blink moving even when a long tool call sends no events. The send
  button turns into a stop square during a run. Context usage comes from the estimated history
  instead of the last request, so it shows up in a freshly loaded session too.

- **The desktop GUI's tool cards show how long the call took**: a running call shows `Elapsed X.Xs`
  to the left of its status label and keeps counting while the turn runs, and a finished one shows
  `Took X.Xs` from the event's `durationMs`, which sessions reloaded from disk recover from the
  stored result. The format matches the terminal UI: seconds under a minute, then `Xm Ys`, then
  `Xh Ym Zs`.

- **The desktop GUI sizes a message bubble to its text**: a short message no longer stretches to the
  full bubble width and leaves the right side empty; the bubble wraps its content, still capped at
  the old maximum so long messages wrap where they used to. The collapse hint and image thumbnails
  count towards that width, so they are never clipped.

- **The desktop GUI remembers whether the session sidebar is open**: `Ctrl-Shift-B`, `/sidebar` and
  the sidebar search box write the choice to `sidebar-gui` in `settings.json` and restore it on the
  next launch; with no key set the sidebar starts open.

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
  `http(s)://` link opens in the system browser on click. Its values wrap instead of being clipped,
  and the table scrolls inside the card (wheel, arrow keys, `PgUp` / `PgDn`, `Home` / `End`) once it
  is longer than fits. The `Ctrl-,` settings panel is the same kind of table now: `Setting / Value /
  Notes`, with the current value in its own column.

- **The desktop GUI's model and thinking pickers say what is what**: the model list gained an
  `Input` column (`text,image`, straight from the model catalog), and every row carries a state
  badge on the right — `default ✓` when the entry is both the saved default and the one in use,
  `default` / `✓` alone otherwise. `Enter` now switches only for this session and `Ctrl-S` writes
  the choice to the settings file (the same split the scoped-model panel already had), so a quick
  switch no longer silently replaces the startup default. The thinking list gains the same badge
  on top of its level descriptions. The TUI's `/model` and `/scoped-models` panels read the input
  types through the same helper, so the two surfaces cannot drift.

- **The desktop GUI answers extension questions**: extension UI requests used to be queued and
  never read, so `ask_user_question` reported "no UI" and the `permissions` / `ssh-remote` prompts
  silently failed closed. The window now polls that queue and handles every request: a `Select`
  request opens a list whose highlighted option is the answer (closing the list answers "cancel",
  so the waiting extension is never left hanging), `ShowOverlay` opens the extension's own panel
  just above the input card — title, fixed header, selectable rows (the highlighted row is kept in
  view when the keyboard moves it), key hints and its inline input row, with keys relayed back as
  overlay events — and `HideOverlay` / window close send `Closed`.
  Notifications land in the transcript (rich spans flattened, custom message cards through their
  extension renderer), `Continuation` / `RestartRun` / `AbortRun` / `RebuildTools` /
  `RewindLastUserInput` map onto the matching agent commands, and `PersistSessionEntry` appends the
  custom entry and acknowledges it.

- **The desktop GUI shows one extension's settings**: `ShowSettings` used to open the global settings
  table, which has nothing to do with the extension that asked. It now opens a panel for that
  extension alone — one row per declared setting with the current value in a `◂ value ▸` cell, `Enter`
  cycling to the next choice and writing it back through the extension (each extension decides
  whether that persists). `/settings <extension>` opens the same panel by hand, which is the only way
  to reach settings whose extension command still needs the terminal.

- **Extension commands are written once and run in both the terminal and the desktop GUI**: a
  command handler now receives a small host surface instead of the terminal's `App` — append a
  message, show the dock panel, read the working directory, ask whether a turn is running — so the
  desktop window executes the same body the terminal does. `/proxy` (toggle the gateway stats dock,
  `listen` / `provider` / `api-key`), `/notify` (`show`, `settled`, `reset`), `/computer`,
  `/shake`, `/agents`, `/doctor`, `/mcp`, `/memory`, `/ponytail`, `/jet`, `/cache-usage`,
  `/hang-detect`, `/codemode`, `/permissions`, `/vet`, `/update`, `/tasks`, `/goal`, `/ssh`,
  `/tunnel`, `/autoresearch`, `/help`, `/plan`, `/todos`, `/usage`, `/debug`, `/rewind`, `/btw` and
  `/recap` all work in the window now, with each host deciding how a message looks (terminal
  system-message stream, window transcript note) and which panel renders a request (`/tasks` opens
  the task menu, `/codemode` opens that extension's settings, `/doctor` shows its report in an
  overlay, `/usage` its quota table). The window keeps native panels for exactly three commands —
  `/skills`, `/extension` and `/settings`, whose browser-style UI exists only there — every other
  command goes through the one registry both hosts share. No command can be written twice.

- **Extensions receive the agent event stream in the desktop GUI**: only the terminal UI used to feed
  every `agent_*` / `turn_end` event to the extension registry, so anything event-driven was dead in
  the desktop window — plan mode never extracted the steps of a plan or asked what to do next, goals
  never settled, the loop / hang detectors never saw a turn, `notify` never announced anything. The
  window now forwards the same stream (with the `hasQueuedMessages` flag the notifier needs on
  `agent_settled`), so those extensions behave as they do in the terminal.

- **Extension one-shot requests run in the desktop GUI**: `/btw`, `/recap`, `/doctor` and `/usage`
  used to answer "no async runtime" there, because extension hooks run on the window's view thread,
  which has no tokio runtime. The window now registers its runtime, so those requests (a side
  question, a recap, provider probes, quota lookups) start and stream their result back into the
  same overlay or message the terminal shows. Extension overlays also repaint while open, so an
  answer that arrives later appears on its own instead of waiting for a key press.

- **The desktop GUI shows the conversation when it opens a session**: history is now handed to the
  view both on startup and after `/resume`, so a reopened session is not an empty window, and
  commands that read context (`/btw`, `/recap`) see the same excerpt the terminal does.

- **The desktop GUI has the dock panel**: extensions that report running work — subagents, task
  streams, downloads, a live ssh-remote or browser session — now show in a strip above the input
  card instead of being invisible. `ShowDock` (what those extensions send when they start) turns it
  on, `/dock` toggles it, sections are separated by a blank line and scroll inside the strip once it
  is taller than a few lines. It disappears on its own when no extension has anything to say, and it
  keeps repainting while something is running, so the subagent elapsed time and counters move. The
  plan-mode todo list uses it too: `/todos` now opens that section instead of pointing at the
  terminal UI.

- **The desktop GUI asks about project trust instead of assuming it**: a project with resources that
  need trust and no saved decision now opens the trust panel at startup (unless `-a` / `--no-approve`
  was given), and `/trust` shows the same dialog with the saved decision marked as current. Options
  are `Trust`, `Trust parent folder (…)`, `Trust (this session only)`, `Do not trust`,
  `Do not trust (this session only)` and `Remove all saved trust decisions` — the last one asks
  for confirmation. The saved options write `trust.json` and apply on the next launch, the two
  "this session only" options reload skills, context files and tools right away.

- **The desktop GUI changes values with `Space`, and `Enter` shows details**: the `/settings` table,
  an extension's own `Settings · <name>` panel, `/extension` and `/skills` all cycle or toggle a row
  with `Space` now instead of `Enter`; the skill and extension panels open their detail page on
  `Enter` (the `Tab` shortcut is gone). Cycling a value inside an extension's settings keeps the
  filter text and the highlighted row, so a search no longer snaps back to the full list. In the
  `/` completion panel a space now completes the command and opens that extension command's declared
  subcommands (`Tab` completes, `Enter` completes and runs), giving `/goal pause`-style second-level
  candidates in the window as well as in the terminal; the panel's description now sits right after
  the command name instead of being pushed to the far edge, so there is room to read it.

- **The desktop GUI accelerates wheel scrolling in the conversation**: the wheel uses the same
  velocity mapping as the terminal UI — an isolated notch scrolls at the system's own step, a fast
  spin multiplies it (up to 10×), and holding `Alt` multiplies it by 5 for jumping through a long
  reply. Only the conversation is affected; menus, panels and the sidebar keep their own scrolling.

- **`/changelog` opens as rendered Markdown**: the update log is laid out like an assistant reply
  — headings, lists, inline code and code blocks instead of one flat block of text — and the panel
  carries a scrollbar, so you can see how far in you are and jump around with `↑` / `↓` / `PgUp` /
  `PgDn` / `Home` / `End`.

- **Images in the desktop GUI are twice as large**: `@image` references and pictures returned by a
  tool render at up to 360 px instead of 180, so a screenshot is readable without opening it.

- **The desktop GUI ships its own documentation**: `docs/gui.md` covers launching it, the layout,
  the sidebar and the conversation area, the right-hand Files / Changes panel, the dock, every
  overlay and dialog, the full key table, the slash commands that work in the window, appearance and
  zoom, and the differences from the terminal interface.

- **`prux-gui --tui` starts in the terminal instead**: the same binary runs the interactive terminal
  interface, reusing the same argument parsing, session and model setup as the GUI. It needs a real
  terminal on both stdin and stdout — piping it out prints an error and exits instead of rendering
  nonsense.

- **The desktop GUI switches its working directory from the keyboard**: clicking the working-directory
  row under the session list always opened the picker; `Ctrl-Shift-W` and a `Working directory…` entry
  in the command palette now do the same, and the row still works.

- **The desktop GUI's keybinding page is a two-column table**: `/hotkeys` puts each key in a left
  column and what it does on the right instead of one sentence per line, with the columns named
  `Keys` / `Action` rather than the generic property / value, and the table scrolls when it outgrows
  the card.

- **Information overlays scroll by dragging their scrollbar**: the thin bar in `/changelog`,
  `/hotkeys` and the detail page of list overlays is now draggable, with a wider hit area than the
  4 px it draws, and the drag keeps working when the pointer leaves the track.

- **The desktop GUI's sidebar filters sessions by title**: a search box sits above the session list and
  narrows it case-insensitively as you type. Click it to start typing (the Files panel's search box
  gives up focus), `Esc` / `Enter` leaves it with the query still applied, and zero matches reads
  `No matching sessions`.

- **The desktop GUI's sidebar is narrower and names its directory**: the default width drops from 252
  to 200 px and the minimum from 180 to 150, so the session list hugs its titles instead of leaving a
  gutter. The working-directory row at the bottom now shows only the last path segment (the full path
  is still what the picker switches to), and the directory menu marks the one you are in with
  `✓ current`.

- **The desktop GUI's pickers are tables instead of one-line entries**: `/models` splits into
  `Model` / `Provider` / `Name` columns, the login panel into `Provider` / `Status` with the
  `configured` mark, and the session picker into `Title` / `Updated` / `Messages`, so the values line
  up down the panel instead of being glued into one label and its grey detail line. `/usage` and
  `/session` read their `key: value` lines into the same two-column table.

- **The `browser` extension gained tabs, history, hovering and page state reads**: `back`,
  `forward` and `reload` walk the tab's own navigation history instead of re-navigating by hand, and
  `close_tab` closes a tab by index or target id. `hover` moves the pointer without clicking
  (drop-downs and tooltips need it), `dblclick` sends two click rounds, and `inspect` reads one piece
  of element state — `value`, `text`, `html`, `attr`, `count`, `visible`, `enabled`, `checked` — so
  the model no longer has to parse the snapshot text for it. `snapshot` takes a `scope` selector and
  a `depth` cap, `wait` accepts a URL fragment or an arbitrary JS expression, and `press`
  understands chords such as `Control+a` and `Meta+v`.

- **The `browser` extension reads the console and answers JavaScript dialogs**: attaching a tab now
  also turns on `Runtime` / `Log` / `Page` event reporting, buffered per tab (200 lines). The new
  `console` action drains that buffer — `console.*` output, uncaught exceptions, failed resource
  loads — and the new `dialog` action reports an open `alert` / `confirm` / `prompt` and accepts or
  dismisses it. A command that times out while a dialog is open now says so instead of looking like
  a dead connection.

- **`/session` takes subcommands, including three that delete sessions**: `/session statistics` is
  the session summary that bare `/session` already printed, and `/session rm-current`,
  `/session rm-project` and `/session rm-all` delete the current session, every session of the
  current project, or every session of every project. Each one asks for a Yes/No confirmation that
  names the number of session files and the directory they live in, and each reports how many
  deletions succeeded. `rm-current` starts a new session afterwards, because the file it just
  deleted would otherwise be recreated by the next write. `/session <n>` still switches to the
  n-th session, so existing habits keep working, and the subcommand names complete in the input box
  in the terminal UI.

- **The desktop GUI copies and pastes on middle click, as the terminal UI does**: with a selection
  (transcript or input box) it goes to the system clipboard and is pasted at the cursor; with no
  selection the clipboard is pasted. Panels have no writable input row, so with one open the click
  only copies.

- **The desktop GUI's input box no longer clips half a character at the end of a wrapped line**: the
  wrap width was the composer card's width minus its horizontal padding, but the card also has a
  1px border on each side, so every visual line was laid out 2px wider than it could be and the last
  glyph was squeezed by the flex row before wrapping to the next line. The border is subtracted now.

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

- **`/debug receipts` answers "what did this session actually do?"**: it reads the records the
  session already keeps, line by line — `tool_started` records for every call, `toolResult` messages
  for exit codes and the `edit` patch that gives exact added/removed line counts, and
  `operation_finished` for turn-level failures — plus the `permissions` review log for the calls
  that **never ran** (`deny` / `ask`), which leave no trace in the session. The receipt leads with
  totals (files changed, commands run, MCP calls, agents, failures, approvals) and then lists the
  turn in order; `/debug receipts <n>` raises the detail cap. Parsing is per line because the
  command is busy-safe: a half-written tail from the running turn is skipped, not fatal.

- **`/debug settings-key` names the settings keys prux does not recognize**: a `settings.json` that
  parses fine but carries a typo (`"compactReserveToken"`, `"theme_gui"`) used to be ignored
  silently — the value simply never took effect. The new subcommand lists every unknown top-level
  key with its `file:line` and, when a known key is within two edits, a `did you mean "…"?` hint;
  nothing to report prints a single confirmation line. Keys that only exist in the global file
  (`theme`, `skills`, …) are flagged when they appear in `.prux/settings.json`, which only honours
  `defaultTools` / `disabledExtensions` / `enabledExtensions`. Grammar errors stay with the startup
  warning; each run re-reads both files.

- **The `delivery` extension is now `pr`, and only `/pr run` hands a change off**: where `vet`
  only proves the local checks pass, `pr` does the rest of the mechanical work when you ask for it —
  branch, commit, push, open a pull request, then report the CI and review result. There is no
  settle-boundary hook and no feedback loop any more: nothing is committed or pushed behind your
  back, so a broken turn never becomes a pull request on its own. Failing checks (`name —
  conclusion`) and `CHANGES_REQUESTED` reviews are listed with the PR link; fix, then `/pr run`
  again — the change lands on the same branch and the same PR is updated. It stops on green, on
  blocked, or when nothing settles within `timeoutSecs` (a pending check is not fed back), and it
  never merges. A `prux/<date>-<time>` branch is created instead of committing onto the base
  branch; a clean tree outside a `prux/` branch does nothing. It will not guess a remote (no
  `origin`, no `gh` login, no detectable default branch all stop with a reason). Config lives in
  `agent_dir()/extensions/pr.json` with no project-level override — a file committed into a
  repository should not be able to change where your work is pushed. `/pr status|run`. Off by
  default, `Dev` mode only: it writes git history and pushes.

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

### Fixed

- **`/pr run` opens a new pull request when the branch's pull request is already closed**: `gh pr
  view <branch>` returns a closed or merged pull request too, so a second `/pr run` on the same
  branch reused it — the pushed commits went nowhere and the run reported the old pull request as
  green. Only an open pull request is reused now; a closed or merged one is announced in the chat
  area and a new pull request is opened. The CI poll addresses that pull request by number instead
  of by branch, so the old one cannot shadow it.

- **The `@` completion panel appears only when it has matches**: the file scan runs in the
  background, so typing `@` (or another character that matches nothing) opened a panel with no
  rows, showed its empty search box, and only collapsed once the scan came back empty — the
  composer jumped every keystroke. The panel is now created from the scan result instead: no
  entries, no panel, same as `/`. A late scan also no longer reopens a panel closed with Esc.

- **Extension panels in the desktop GUI redraw only when their content changes**: an open `/btw`,
  `/doctor` or other extension overlay re-fetched its view ten times a second and asked the window
  for a new frame every time, even with nothing moving. A frame now counts as a redraw only when a
  field the GUI actually renders (title, header, lines, footer, selection, input focus or the input
  buffer) differs from the previous one, so an idle panel no longer flickers.

- **The desktop GUI window is opaque on Wayland**: it asked for client-side decorations, which the
  windowing layer treats as a transparent window — the surface was handed to the compositor with
  premultiplied alpha and without an opaque region, so a full-window repaint flickered. Decorations
  now come from the server, so the surface is declared opaque and carries an opaque region.

- **The desktop GUI's session right-click menu highlights one row at a time**: the keyboard
  selection and the mouse hover were drawn from separate states, so hovering `Delete` left `Rename`
  lit as well and the menu showed two highlights at once. Hovering now moves the single selection,
  as the arrow keys do, and the colour follows it.

- **Deleting the last session of a project no longer leaves an empty folder behind**: every project
  keeps its sessions in a per-project folder under `sessions/`, and `/session rm-current`,
  `rm-project`, `rm-all` and the GUI sidebar's Delete removed the files but kept the folders. The
  folder now goes with the last session in it (`/new` and the GUI's `+` also stopped pre-creating
  it), so the directory listing matches what is actually there.

- **The cursor at the end of a full line in the desktop GUI's input box no longer pushes a glyph
  out of the card**: the block was appended to the line that was already filled edge to edge, so it
  hung over the card's border. It now gets its own visual line when the current one has no room
  left, and each glyph's width is rounded up to whole physical pixels — the layout engine places
  per-character elements at integer pixels, so the fractional remainder used to let every visual
  line hold one glyph too many. The card is also handed to the measuring layer wrapped, so the
  measured width is the card's own and not its inner input row's.

- **`/session` subcommands complete in the desktop GUI's input box**: only extension commands could
  declare subcommands, so `/session` offered nothing. The built-in `statistics`, `rm-current`,
  `rm-project` and `rm-all` are listed now, and they win over an extension command of the same name,
  matching how the command itself is dispatched.

- **The desktop GUI's `/history` panel uses the full row width**: prompt rows have no right-hand
  detail, but they were still laid out as "label capped at the detail column width + detail", so
  every prompt was truncated at roughly a quarter of the panel with the rest of the row left blank.
  A row with an empty detail now lets the label take the whole width; `/command` and `@path` rows,
  which do carry a description, keep the description right after the name.

- **The desktop GUI only renders the part of a conversation you can see**: a long session used to
  rebuild every transcript block on each redraw, which cost tens of milliseconds per frame and made
  the window flicker while anything was moving. Blocks more than a screen away from the viewport are
  now replaced by a placeholder of their measured height, so a redraw only pays for what is on
  screen; their line entries are replayed from the previous frame, so selection and copy still see
  the whole text. A block that changes while off screen keeps its old height until it scrolls back in.

- **The desktop GUI no longer repaints itself every frame**: the transcript's measure-and-pin
  callbacks compared layout results with exact floats and asked for a new frame whenever they
  differed — on a long conversation the accumulated subpixel error never matched, so the whole
  window redrew continuously and flickered even when nothing moved. The comparisons now allow
  half a pixel, and each rendered line builds its text and highlights once instead of twice.
  `PRUX_GUI_FRAMES=1` makes the GUI report its frame interval to stderr, which shows whether
  anything is still asking for a redraw while idle.

- **The desktop GUI no longer re-parses an overlay's Markdown on every frame**: `/changelog` and
  the other Markdown panels ran the whole document through the parser and the syntax highlighter on
  every redraw, so scrolling a long changelog stuttered. The parsed rows are cached and reused until
  the text, the light/dark theme or the layout width changes — assistant bodies already worked this way.

- **A new session's file path reaches the UI once it lands on disk**: `/new` starts a deferred
  session, so the switch reply has no path — and nothing ever sent one later. Until the next switch
  `/fork` answered `no current session`, `/resume` did not mark the running session as current, and
  extensions keyed the session as `default`. The worker now reports the path the moment the first
  message writes the file, and the switch is re-dispatched to extensions with it.
- **The desktop GUI renders tables, formulas and Mermaid diagrams**: assistant markdown used to
  run a table's cells together into one paragraph, leave `$...$` / `$$...$$` as literal source and
  print `mermaid` fences as plain code. Tables now render as a real grid (column widths follow each
  column's longest cell, cells wrap), inline and display math go through the same LaTeX-to-Unicode
  renderer the terminal UI uses, and `mermaid` blocks become the same box-drawing diagrams. The
  LaTeX and Mermaid renderers moved into the core layer so both UIs share one implementation.

- **The desktop GUI's status dots sit on the text's optical center**: the brand dot next to `prux`
  (and the tool card's status dot) was centered on the line box, which left it about 2 px above the
  middle of the letters. It is nudged back down so the dot and the lowercase text share one horizontal
  center line.

- **Desktop GUI slash commands keep their arguments**: `/compact <instructions>` and `/bug <hint>`
  used to be swallowed as an unknown command, `/history` parked its text back in the input box
  instead of reopening the search panel, and `/name <name>` renamed the session while the sidebar
  kept showing the old title.

- **`/rewind` works in the desktop GUI**: the command went straight to the agent, so the branch
  moved back but the transcript kept showing the input and everything after it. It now runs the same
  confirmation panel as the terminal UI (rewind & restore the input to the box / rewind & discard /
  cancel) and rebuilds the transcript from the trimmed history.

- **Images in the desktop GUI keep their aspect ratio**: `@image` references and pictures returned by
  a tool used to sit in a fixed square, so a wide screenshot left empty bars above and below. The
  thumbnail is now sized from the image itself — small pictures render at their own size, large ones
  scale down to the 360 px cap without distortion — and formats the decoder cannot measure fall back
  to the old square.

- **An image in the desktop GUI stays the picture it was when sent**: `@image` references used to be
  re-read from their path on every frame, so rewriting that file (saving a screenshot under the same
  name) swapped the thumbnail in messages that had already gone out. Each user message now keeps a
  content-addressed snapshot taken when it was sent, thumbnails are built from the file bytes so the
  image cache keys on the content instead of the path, and temporary files are named by a hash of the
  whole content — the old "length + first byte" name collided between different pictures and kept
  showing the stale one.

- **The desktop GUI's status bar keeps moving while a turn runs**: usage used to arrive only with the
  end-of-turn snapshot, so context window, cache hit rate, in/out and cost stood still through the
  whole turn — a fresh session read `0% · CH 0%` until the turn finished. They are now advanced from
  agent events as each message lands: the assistant's usage anchors the context size, later messages
  and tool results are estimated on top, and a compaction resets the anchor to the size it reports.
  The message count still steps aside while a turn runs because `Working…` already leads the row.

- **`Ctrl-W` no longer swallows `Ctrl-Shift-W`**: the delete-word-back shortcut matched any
  `Ctrl-W`, so the new working-directory panel key was eaten by the input box; it now requires the
  shift key to be up.

- **File edits in the desktop GUI render as diffs**: the `edit` card shows the file path in its
  header and the change itself as a diff — a right-aligned line-number gutter, added lines in the
  success colour and removed lines in the danger colour, `@@` hunks and context lines muted, and a
  single-line replacement highlighted word by word. The diff comes from the tool result
  (`details.diff`) and falls back to a preview computed while the call is still running, so the card
  fills in before the edit finishes. `write` cards keep the path in the header (instead of dumping
  the whole file body into the collapsed summary) and preview the content being written in the
  body — the first few lines plus a "click to expand" hint, the same shape the terminal UI uses.

- **A fresh desktop GUI session follows new output again**: scrolling was only pinned after the
  view had been seen at the bottom once, and content shorter than the viewport counted as "scrolled
  away" because the target offset was positive. New output in a brand-new session therefore grew
  below the fold until you scrolled down once by hand. The pin now starts engaged and clamps the
  target to the real scroll range, so the first turn follows to the bottom and only a real scroll up
  stops it.

- **The desktop GUI's `/` panel lists every command it can run**: `hotkeys`, `session`, `changelog`,
  `trust`, `bug`, `theme`, `reload`, `settings`, `extension`, `skills`, `scoped-models`, `login`,
  `logout`, `usage` and `debug` were executable but unlisted. `/name <text>`, `/thinking <level>`,
  `/model <id>`, `/session <n>`, `/export <path>` and `/import <path>` now take their argument
  instead of being read as an unknown command (`/model` matches inside the scoped model list when one
  is set, `/session` indexes the session directory in the same most-recent-first order the sidebar
  uses and reports the valid range otherwise), and `/theme` and `/hotkeys` are aliases for the
  appearance picker and the keybinding page. Enter in the `/` panel with no candidates completes
  what was typed instead of doing nothing, which is what made an argument command look stuck on
  "No matches".
  The `Ctrl-M` command palette keeps only what the `/` panel does not cover, plus `skills`,
  `extension` and `settings` — the three with their own panels — so the same action no longer has
  two entries competing for the same keystrokes.

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

- **Selecting text keeps the inline styles underneath it**: a transcript highlight has to be split
  where it meets inline code or a link, so the selection background no longer swallows those words
  and paint over only part of the selection.

- **`/cache-usage all` no longer freezes the desktop GUI**: the scan ran on the view thread
  whenever no async runtime was in scope, so with many saved sessions the whole window stopped and
  the panel appeared only after the scan had finished. The scan now runs in the background and the
  panel opens right away with `scanning sessions…`.

- **Arrow keys scroll the desktop GUI's info panels**: the scroll offset was clamped against a
  range with the wrong sign, so `↑` / `↓` / `PgUp` / `PgDn` on `/usage`, `/help`, `/session`,
  `/changelog` and the extension detail pages snapped back to the top instead of moving. `End` goes
  to the bottom now.

- **`/rewind` works in the desktop GUI**: it was listed in the `/` panel but had no handler, so the
  command text simply went back into the composer. It now rolls back the last user input.

- **`/debug` answers in the desktop GUI**: it was listed in the `/` panel but did nothing. A bare
  `/debug` now prints the agent diagnostics into the conversation (provider, model, message count,
  auto-compaction and retry, working directory, session); the `/debug info` / `provider` /
  `system-prompt` subcommands stay with the `debug` extension.

- **The desktop GUI forgets working directories that no longer exist**: the recently-used list in
  the working-directory picker only ever grew, so moved or deleted projects stayed in the menu as
  entries that could not be opened. Startup now drops the paths that are gone and writes the list
  back, and a directory change is detected against the incoming snapshot's `cwd` before that
  snapshot replaces the old one.

- **Commands in the desktop GUI's `bash` cards wrap instead of being cut**: the command sat in the
  header as a one-line summary with a `command=` prefix, truncated and squeezed against the status
  label. It now takes the second line of the card, wraps, and uses the code colour; the lines it
  occupies come out of the output line budget.

- **Blank assistant turns leave no gap**: a turn that made a tool call but produced neither text nor
  thinking rendered as an empty block, and the flow's `gap` still counted it, so neighbouring blocks
  floated apart. Empty turns are skipped when rendering.

- **`Enter` on an empty composer jumps back to the newest output**: pressing `Enter` with nothing
  typed used to do nothing while the view was scrolled up; it now re-pins the transcript to the
  bottom.

- **A tool card shows one "N more lines" hint, not one per section**: a `bash` card could stack a
  hint under the command, another under the diff or written body, and a third under the output,
  each counting only its own overflow. The hidden lines are now summed and the card ends with a
  single hint.

- **The desktop GUI's `@` panel finds files the way the terminal UI does**: it used to filter a
  one-shot list of files (depth-limited, files only) by substring, so directories, deeper files and
  hidden files never came up and the order did not match. The panel now runs the same background
  directory walk with the same path scoring: directories are candidates (completing to `dir/` so you
  can keep typing into them), `.gitignore` / `.ignore` / `.fdignore` are honoured, hidden files are
  listed, results rank by score then by depth, length and name, and a path that contains spaces or
  CJK punctuation is completed as a quoted path. Typing a directory prefix scopes the walk to it
  (`@src/mo` only walks `src`), a `@` that starts a word after `(`, `` ` `` or CJK punctuation
  triggers the panel while the one in `user@example.com` does not, and continuing to type cancels the
  previous walk.

- **The desktop GUI's `/history` lists prompts as you type**: typing `/history` (or `/history bug`)
  now opens the completion panel above the input box with this project's prompts, most recent
  first — no Enter needed to see anything. The query is split on whitespace into case-insensitive
  substrings that must all appear anywhere in the full prompt, in any order, so a prompt whose text
  sits on later lines is still found. Enter / Tab writes the selected prompt back into the input box
  (multi-line prompts come back as multiple lines) without sending it, and an empty result keeps the
  panel open instead of sending `/history …` as a prompt. `@clear` / `@clear-all` (visible only while
  the query starts with `@`) delete this project's history or every project's, behind a Yes/Cancel
  confirmation that shows how many entries are at stake.

- **The desktop GUI's completion panel closes when nothing matches**: `/command` and `@path` used
  to leave an empty `No matches` box on screen, unlike the terminal UI, which hides the panel. It now
  closes as soon as the command, subcommand or file scan has no candidate, and the characters typed
  into the panel come back into the input box instead of vanishing with it — `Esc`, backspacing past
  the prefix and an empty result all leave the box holding what was actually typed. The `/history`
  panel is the one exception: a query that matches nothing keeps it open as a search box and says
  `No matching history`. Leading indentation before a `/` now triggers the panel too, as it does in
  the terminal UI.

- **Expanding a card in the desktop GUI no longer flickers**: the toggle changed the height, the
  pointer was still over the card when the button came up, and the second hit flipped it straight
  back. One entry now accepts a single toggle per 250 ms, and a toggle clears the previous frame's
  measured height so the layout is not pinned to the old one.

- **The desktop GUI's completion panel no longer pops up on cursor movement**: `/command` and
  `@path` were recomputed after every key, so pressing the arrow keys in the middle of a prompt
  reopened the panel over text that had not changed. Suggestions are now refreshed only when the
  editor's text actually changed.

- **An aborted turn in the desktop GUI really stops everything**: `Esc`, the stop button and an
  extension's abort now report `agent_end` (as the terminal UI does), so event-driven extensions
  such as the idle-timeout watchdog stop believing the turn is still running and no longer restart
  an already idle session. They also drop cancellable background work (an MCP OAuth login, say)
  and take the queued steer / follow-up messages back into the input box; `Esc` does that before
  menu and overlay handling while a turn is running.

- **Restarting a turn in the desktop GUI no longer drops queued input**: steer messages that piled
  up while an extension rebuilt the prompt are now sent with the new prompt instead of being lost.

- **The `browser` extension no longer clicks through a banner**: `click` and `dblclick` check the
  element's center point first and refuse when another element covers it (naming it, e.g.
  `covered by <div#banner>`), when the element is disabled, or when it has no clickable area. The old
  behaviour dispatched the click anyway and reported success.

- **`Ctrl-N` no longer starts a new session in the desktop GUI**: it collides with the terminal UI's
  named-session filter, so the shortcut is gone; use `/new` or the `+` button in the sidebar.

- **Long overlay and dock lines wrap instead of running off the card in the desktop GUI**: each line
  was a flex row of per-span elements, and a text element's automatic minimum width is its whole
  unwrapped line — so the row never shrank and long text (a subagent's task, a dock status line)
  painted straight past the panel's border. The spans now let themselves shrink, and such lines
  break inside the card.

- **Deleting a session in the desktop GUI goes to the trash, as in the terminal UI**: the sidebar's
  Delete action unlinked the file outright, so one misclick was unrecoverable. It now runs the
  `trash` CLI first and falls back to unlinking only when that fails, and it refuses to delete the
  session that is currently open — that file is still being appended to, as the terminal UI already
  enforced. The confirmation no longer claims the deletion cannot be undone.

- **`Esc` in the desktop GUI closes an open panel before it aborts the turn**: while a turn was
  running, `Esc` was checked against the busy state first, so hiding an overlay or menu cancelled
  the running turn instead. The busy-state abort now runs after the overlay, dropdown, completion
  panel and search boxes, matching what the terminal UI already did.

- **`leak-guard` no longer flags paths and session file names, and stops repeating itself**: the
  high-entropy scan treated `/` as part of a candidate string, so any file path became one long
  'random' blob, and UUID / hash / `2026-11-02T10-25-54-666Z_<uuid>` session names went through as
  well. Paths no longer join into a candidate, machine-generated identifiers are skipped, and the
  warning memory now drops its oldest entry instead of wiping itself — a history full of such
  names used to reset the memory every request, so the same warning came back every turn.

- **`leak-guard` now names the snippet that triggered it**: the warning only said "looks like
  credentials (high-entropy)", so a path, a base64 blob and a hash were indistinguishable. The
  prompt and tool-argument warnings now carry a short head/tail preview of the hit (for example
  `high-entropy: q8Zr2NvT…Zx4Q`). Blocked tool calls still report the category only, because
  that reason goes back into the model context.

- **A brand-new session in the desktop GUI now shows up in the sidebar and gets highlighted**: the
  file is only written on the first message, and the sidebar was only re-read when the working
  directory changed or a session was renamed, so the new row stayed missing and nothing was
  highlighted. A session path appearing or changing now re-reads the list.

## 中文

### 新增

- **`prux acp` 把 prux 变成编辑器的 agent 后端**：新增 `acp` 扩展注入 `acp` 子命令——`bootstrap()`
  把**已装配好的** Agent 交给它，因此编辑器拿到的是与 TUI 完全相同的模型链、工具表、会话文件与
  扩展钩子，而不是第二套 agent loop。它在 stdin/stdout 上讲 NDJSON + JSON-RPC 2.0，应答
  `initialize` / `authenticate` / `session/new` / `session/prompt` / `session/cancel`，把文本、
  思考、工具调用起止以 `session/update` 通知流式推给客户端。回合执行期间同时监听 stdin，取消
  即时生效，不会排在一个慢回合后面。单会话（`session/new` 返回 `bootstrap()` 已打开的会话），
  不支持 `session/load`，prompt 只取文本块。该扩展不注册工具、不订阅钩子——不开 `prux acp`
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


- **桌面 GUI 的每个代码块右上角都有复制按钮**：点击代码块首行右侧的复制图标，整块源码进剪贴板，
  并弹出 `Copied to clipboard` 提示。

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

- **新增 `hardware` 扩展：本地芯片手册与引脚表检索**：把转成 Markdown 的手册（和引脚 CSV）放进
  `agent_dir()/extensions/hardware/datasheets/`，可以按芯片建子目录；`hardware_search` 用与其它
  检索同一套词法切词给每个块打分，返回 `file:line` + 片段正文。命中在路径里的块额外加权，所以搜
  `usart` 会优先出 `stm32f4/…` 而不是无关文件；`path` 收窄前缀、`limit` 限条数。引脚别名不需要
  第二套机制——引脚表的一行（`PA9,USART1_TX,TIM1_CH2`）就是一块，引脚名和复用功能名都能搜到。
  资料目录每次调用重扫（不建索引缓存）。
- **`hardware` 扩展同时直连板子**：探针通道走 `probe-rs`——`hardware_board_info` 读芯片名与架构、
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
  跳过，所以 commit 与内容哈希不会误报）。档位 `warn`（默认）只提示；`block` 只把**本次请求**
  的命中替换成 `[REDACTED:<类别>]`，并拒绝携带凭据的工具调用、把命中的类别回给模型。思考块只扫
  不改——它的文本带签名，改了签名就失效。同一 `(类别, 片段)` 只提示一次。档位存在
  `agent_dir()/extensions/leak-guard.json`——`/leak-guard warn|block|off` 设档，裸 `/leak-guard` 打开扩展设置面板。默认不启用（`Dev` / `Creator`）。

- **新增 `processes` 扩展：后台进程监督**：内置 `bash` 工具只有「前台执行 + 超时杀进程」，
  dev server、watcher 这类长命命令会把一整轮卡到超时。`background_bash` 把命令丢到后台，立刻
  返回 `job-N`、pid 与日志路径——stdin 接 `/dev/null`，stdout/stderr 写进那个日志文件而不是
  继承终端。`read_process_output` 返回输出**快照**（状态、pid、耗时、最后 N 行；读不消费日志，
  省略 `id` 列出全部 job），`stop_process` 杀掉整个进程组。`/ps` 三段式切换停靠段显隐
  （同 `/todos`），`/ps list` 打印清单，`/ps stop <id>` 停止、`/ps clear` 丢掉已完成记录。停靠段只列
  运行中的 job——刚结束的行在下一帧被刷掉，全部结束时段落与面板自动收起。job 不随回合结束消失，
  禁用本扩展会停掉全部运行中的 job。默认不启用（`Dev` 模式）。

- **桌面 GUI 的侧栏分组标题显示该组条目数**：`Today` / `This week` / `Older` 右侧按说明色右对齐
  显示条数，随搜索过滤结果变化，数的是列表里真实呈现的会话。

- **桌面 GUI 的 `/session` 面板支持复制会话文件路径**：`Session file` 一行可点击（链接色、悬停
  下划线），点击把路径写进系统剪贴板并弹 toast 确认；内存会话没有路径，保持普通文本。

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

- **`computer_crop` 放大已有的图**：`computer` 扩展新增第 15 个工具，把磁盘上的图片文件按
  `[x, y, width, height]` 裁出来返回——想看清截图里的某一块不必重新截屏。像素取自落盘的原图，
  所以即便交付给模型的那张图被缩过，文字依旧清晰；矩形超出图片范围会连真实尺寸一起报错并拒绝，
  回执里给出偏移量，裁出来的点可以换算回桌面坐标。

- **Rex 终端被认出来了**：`TERM_PROGRAM=rex` 下内联图片走 kitty 协议（不再退化成半块字符），
  配色按 truecolor 处理，超链接与图片都能正常显示，而不是退成 `text (url)`。

- **桌面 GUI 也能只用键盘切模型与思考级别**：`Ctrl+P` / `Ctrl+Shift+P` 在模型 scope 里前后循环
  （scope 来自 `--models` / `settings.enabledModels`，为空时才退回全部已配置模型；顺序跟 scope、
  跳过目录里已经没有的条目，条目带 thinking 时先设思考级别再换模型）；`Shift+Tab` 循环当前模型
  支持的思考级别。两个键都与 TUI 的默认绑定一致。

- **桌面 GUI 的会话搜索框有了快捷键**：`Ctrl+Shift+H` 聚焦左侧会话列表上方的搜索框（侧栏收起时
  一并展开，右侧面板的搜索框让出焦点），`Esc` / `Enter` 退出聚焦并保留查询。

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
  浏览历史时不弹补全面板，翻出 `/model` 这类历史也不会把 `↑` / `↓` 抢走。命令面板里这一项
  叫 `Sidebar`。

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

- **桌面 GUI 的右侧面板能横竖滚动**：diff 与文件正文各自记住自己的滚动位置，右缘一根纵向条、
  底缘一根横向条；在轨道任意位置按住拖动都能滚，指针中途跑出轨道也不断。长行不再折行，横向条
  才有的可滚；换文件或换变更时该片内容回到顶部，不再沿用上一个文件的偏移（否则新内容一上来就
  被顶到面板外）。

- **桌面 GUI 的状态栏汇报这一轮，而不只是消息数**：左侧是消息数、上下文窗口占用率与缓存命中率，
  右侧是 in/out token、费用、每秒输出速率，以及当前是否有目标或计划、已发交互数与本轮耗时。
  忙碌时行首先亮一个闪烁圆点与 `Working… (Esc to abort, N queued)`，并每半秒重绘一次，长工具
  调用期间没有事件时耗时也照样走；发送按钮在忙碌时变成停止方块。上下文占用改用历史估算而不取
  最后一次请求，刚载入的会话也能看到占用率。

- **桌面 GUI 的工具卡片显示这次调用花了多久**：跑着的调用在状态标签左侧显示 `Elapsed X.Xs`，
  回合期间一直往上走；结束后定格成事件里的 `durationMs`（`Took X.Xs`），从磁盘重载的会话也能
  从存档结果里恢复。格式与终端 UI 一致：不到一分钟按秒，再往上 `Xm Ys`、`Xh Ym Zs`。

- **桌面 GUI 的消息气泡按文字长度收缩**：短消息不再撑满整个气泡宽度、右侧留一大片空白；气泡宽度
  由内容决定，上限不变，长消息照旧在那里折行。折叠提示与图片缩略图也是气泡的子项，不会被裁。

- **桌面 GUI 记住会话栏是否展开**：`Ctrl-Shift-B`、`/sidebar` 与侧栏搜索框把选择写进 `settings.json`
  的 `sidebar-gui`，下次启动照原样恢复；没写过时默认展开。

- **桌面 GUI 记住自己选的明暗主题**：`Shift+T` 与 `/settings` 的外观面板把选择写进 `settings.json`
  的 `theme-gui`，下次启动照样恢复，选过一次之后窗口主题就不再跟着 `--theme` 走；终端里的 `theme`
  是另一个键，互不影响。

- **桌面 GUI 的技能、扩展与模型名单面板改成表格**：有表头行，名称、来源（扩展是可用模式、模型是
  provider）与描述各占一列定宽，不再把 `描述 · 来源 · [model-off]` 拼成一串挤在行尾，行与行对得齐，
  长文本按单元格截断（`Tab` 详情页仍显示完整路径）。模型名单面板也跟另外两个对齐：`Enter` 只切换
  草稿里的勾选框、只有 `Ctrl-S` 才写回 `enabledModels`，标题里带启用数量，`Esc` 丢弃草稿。`Tab` 详情页
  同样是 `Property / Value` 两列表格，`fork url` 这类 `http(s)://` 链接点一下就交给系统浏览器打开。
  详情页的值按列宽折行、不再截断，内容长过卡片时在卡片内部滚动（滚轮、方向键、`PgUp` / `PgDn`、
  `Home` / `End`）。`Ctrl-,` 设置面板也改成同样的表格：`Setting / Value / Notes` 三列，当前值单独
  占一列。

- **桌面 GUI 的模型与思考级别选择器标清了默认值和当前值**：模型列表多一列 `Input`
  （`text,image`，直接取自模型元数据），每行右侧带状态标记——既是保存的默认又在用是 `default ✓`，
  只有其一是 `default` / `✓`。`Enter` 改成只在本次会话切换，`Ctrl-S` 才写进设置文件（与模型名单
  面板同一套分工），随手换模型不再悄悄改掉启动默认值。思考级别列表在原有的级别描述之外也带上
  同样的标记。TUI 的 `/model` 与 `/scoped-models` 面板经同一个函数取输入类型，两处展示不会漂移。

- **桌面 GUI 能回答扩展的提问了**：扩展发出的 UI 请求以前只入队没人取，`ask_user_question`
  直接报「没有 UI」，`permissions` / `ssh-remote` 的确认也一律按拒绝处理。窗口现在定时轮询这个
  队列并处理每一类请求：`Select` 打开一个列表，高亮项就是答案（关掉列表等于取消，发起方不会
  一直挂着）；`ShowOverlay` 在输入卡片上方弹出扩展自己的面板——标题、固定区、可选行、键位提示与
  内联输入行（键盘移动选中行时会把它滚进视口；输入行里 `Enter` 保存、`Esc` 只丢弃这次编辑而不是
  关掉面板，`i` 聚焦输入行、方向键滚动内容等默认语义与终端 UI 一致），按键按覆盖层事件回传；`HideOverlay` 与关窗回传 `Closed`。通知落进对话流（富文本
  摊平成文本，自定义消息卡走扩展自己的渲染器），`Continuation` / `RestartRun` / `AbortRun` /
  `RebuildTools` / `RewindLastUserInput` 分别对应到 agent 命令，`PersistSessionEntry` 追加自定义
  条目并回执。

- **桌面 GUI 能单独显示某个扩展的设置**：`ShowSettings` 以前打开的是全局设置表，跟发起它的扩展
  毫无关系。现在只为该扩展开一个面板——每个设置项一行，当前值放在 `◂ 值 ▸` 里，`Enter` 循环到
  下一个候选并回写给扩展（是否落盘由扩展自己决定）。`/settings <扩展名>` 可以手动打开同一个面板，
  这也是那些「命令仍需终端」的扩展唯一能改设置的地方。

- **扩展命令只写一份，终端与桌面 GUI 都能跑**：命令处理函数现在收到的是一个小小的宿主能力面
  （追加一条消息、显示停靠面板、读工作目录、问是否正在输出），而不是终端的 `App`，于是窗口执行
  的就是终端那一份正文。`/proxy`（开关网关状态面板，以及 `listen` / `provider` / `api-key`）、
  `/notify`（`show` / `settled` / `reset`）、`/computer`、`/shake`、`/agents`、`/doctor`、`/mcp`、
  `/memory`、`/ponytail`、`/jet`、`/cache-usage`、`/hang-detect`、`/codemode`、`/permissions`、
  `/vet`、`/update`、`/tasks`、`/goal`、`/ssh`、`/tunnel`、`/autoresearch`、`/help`、`/plan`、
  `/todos`、`/usage`、`/debug`、`/rewind`、`/btw`、`/recap` 现在在窗口里都能用，消息长什么样由宿主决定
  （终端是系统消息流，窗口是对话流里的一条提示），要面板时也由宿主渲染（`/tasks` 打开任务菜单、
  `/codemode` 打开该扩展的设置、`/doctor` 把报告放进覆盖层、`/usage` 显示额度表）。窗口只对三条命令
  保留原生面板——`/skills`、`/extension`、`/settings`，那种浏览式界面只有窗口有；其余命令一律走两个
  宿主共用的同一张注册表，任何命令都不必写两遍。

- **桌面 GUI 把 agent 事件流转发给扩展**：以前只有终端 UI 会把每个 `agent_*` / `turn_end` 事件喂给
  扩展注册表，于是所有「事件驱动」的扩展在桌面端都是死的——plan mode 不会抽取计划步骤、也不会问
  下一步做什么，目标不会结算，loop / hang 检测看不到回合，`notify` 也不会发通知。现在窗口转发同一份
  事件流（并在 `agent_settled` 上补 `hasQueuedMessages`，通知扩展要靠它区分「真空闲」与「还有排队
  输入」），这些扩展的行为与终端一致。

- **扩展的一次性请求在桌面 GUI 里能跑了**：`/btw`、`/recap`、`/doctor`、`/usage` 以前在窗口里只回
  「no async runtime」——扩展钩子跑在窗口的视图线程上，那里没有 tokio 运行时。现在窗口会登记自己的
  运行时，这些请求（旁问、回顾、provider 探针、额度查询）能起跑，并把结果送回终端里同一个覆盖层或
  消息区。扩展覆盖层打开期间也会持续重绘，晚到的答案自己出现，不用等按键。

- **桌面 GUI 打开会话时会显示对话内容**：历史在启动与 `/resume` 之后都会交给视图，重开的会话不再是
  空窗口；读上下文的命令（`/btw`、`/recap`）拿到的摘录也与终端一致。

- **桌面 GUI 有了停靠面板**：会报告运行中工作的扩展——子代理、任务流、下载、连着的 ssh-remote
  或浏览器会话——现在显示在输入卡片上方的一条面板里，而不是完全看不见。扩展启动时发的 `ShowDock`
  会把它打开，`/dock` 切换开关；各段之间空一行，内容超过几行时在面板内部滚动。没有扩展有话要说时
  它自动消失，运行期间持续重绘，所以子代理的耗时与计数会动。计划模式的待办也走这里：`/todos` 现在
  直接打开该段，不再提示「去终端 UI 看」。

- **桌面 GUI 会先问项目信任，而不是当作已信任**：项目里有需要信任的资源、又没有存过决策时，
  启动就弹信任面板（`-a` / `--no-approve` 显式给过决定时不弹），`/trust` 打开同一个对话框并把
  已保存的决策标成 current。选项有 `Trust`、`Trust parent folder (…)`、`Trust (this session
  only)`、`Do not trust`、`Do not trust (this session only)` 与 `Remove all saved trust
  decisions`——最后一项会再确认一次。落盘的选项写进 `trust.json`、下次启动生效，两个「仅本次」
  选项则立即重载技能、上下文文件与工具。

- **桌面 GUI 改用 `Space` 改值、`Enter` 看详情**：`/settings` 表格、扩展自己的 `Settings · <名称>`
  面板、`/extension` 与 `/skills` 现在都用 `Space` 切换一行的取值，不再用 `Enter`；技能与扩展面板的
  详情页改由 `Enter` 打开，`Tab` 这条快捷方式不再保留。扩展设置面板里循环取值会保留过滤框内容与
  高亮行，搜索之后不再跳回完整列表。`/` 补全面板里敲空格会先补全命令名并弹出该扩展命令声明的
  子命令（`Tab` 只补全，`Enter` 补全并执行），桌面端也有了 `/goal pause` 这样的二级候选；
  面板里的说明紧跟命令名而不是被推到最右边，描述才显示得下。

- **桌面 GUI 的消息区支持滚轮加速**：滚轮沿用终端 UI 的那套速度换算——孤立一格按系统自己的步长
  滚，快速连滚会按间隔放大（最多 10 倍），按住 `Alt` 再乘 5，用来快速跳过很长的回复。只有对话区
  受影响：菜单、面板与侧栏还是各自的滚动。

- **`/changelog` 按 Markdown 渲染并带滚动条**：更新日志现在像助手回复一样排版——标题、列表、行内
  代码与代码块都有样式，不再是一整块纯文本；面板右侧显示滚动条，能看到自己在哪一段，也能用
  `↑` / `↓` / `PgUp` / `PgDn` / `Home` / `End` 翻。

- **桌面 GUI 的图片放大了一倍**：`@图片` 引用与工具返回的图片最大边长从 180 提到 360，截图不用
  点开也看得清。

- **桌面 GUI 有了自己的使用文档**：`docs/gui.md` 说明如何启动、布局、侧栏与消息区、右侧的
  Files / Changes 面板、停靠面板、各类覆盖层与对话框、完整键位表、窗口里能用的斜杠命令、
  外观与缩放，以及与终端界面的差异。

- **`prux-gui --tui` 改为启动终端界面**：同一个二进制能跑交互 TUI，参数解析、会话与模型装配与
  桌面端完全共用。它要求 stdin 与 stdout 都是真实终端，被管道接走时直接报错退出，不会画出乱八
  糟的东西。

- **桌面 GUI 用键盘切换工作目录**：会话列表底部那行工作目录一直点得开选择面板，现在 `Ctrl-Shift-W`
  与命令面板里的 `Working directory…` 也能打开，那一行照旧可点。

- **桌面 GUI 的键位页是两列表格**：`/hotkeys` 把键位放左列、动作放右列，不再一行一句，表头是
  `Keys` / `Action` 而不是通用的属性 / 值；行数超过卡片高度时表格内部滚动。

- **信息覆盖层的滚动条可以拖动**：`/changelog`、`/hotkeys` 以及列表覆盖层的详情页右侧那根细条
  现在能抓来拖，命中区比画出来的 4px 宽，指针离开轨道也继续跟着走。

- **桌面 GUI 的侧栏可以按标题筛会话**：会话列表上方多了一个搜索框，边敲边按标题大小写不敏感
  过滤。点它开始输入（Files 面板的搜索框同时让出焦点），`Esc` / `Enter` 退出聚焦但查询仍然生效，
  零匹配显示 `No matching sessions`。

- **桌面 GUI 的侧栏更窄，底部目录只显示末级名**：默认宽度从 252 降到 200px，最窄从 180 降到
  150px，会话列表不再留一条空白。底部那行工作目录只显示路径的最后一段（选择面板切换的仍是完整
  路径），目录菜单给当前所在目录标上 `✓ current`。

- **桌面 GUI 的选择面板改成分列表格**：`/models` 拆成 `Model` / `Provider` / `Name` 三列，登录面板是
  `Provider` / `Status`（`configured` 标记），会话选择器是 `Title` / `Updated` / `Messages`——各项沿列对齐，
  不再把值挤进一行标题加一行灰色详情。`/usage` 与 `/session` 的 `key: value` 行也归到同一套两列表格里。

- **`browser` 扩展补上标签页、历史、悬停与页面状态读取**：`back` / `forward` / `reload` 走标签页自己的
  导航历史，不必再手写一次导航；`close_tab` 按索引或目标 id 关标签页。`hover` 只移动指针不点击（下拉
  菜单与 tooltip 需要），`dblclick` 发两轮点击，`inspect` 读元素的单项状态——`value` / `text` /
  `html` / `attr` / `count` / `visible` / `enabled` / `checked`——不用再从快照文本里猜。`snapshot`
  支持 `scope` 限定子树与 `depth` 限制层数，`wait` 支持 URL 片段与任意 JS 条件，`press` 支持
  `Control+a` / `Meta+v` 这类组合键。

- **`browser` 扩展能读控制台、能处理 JS 对话框**：attach 一个标签页时顺带打开 `Runtime` / `Log` /
  `Page` 的事件上报，按标签页缓冲（200 条）。新的 `console` 动作取走并回放缓冲——`console.*` 输出、
  未捕获异常、资源加载失败；新的 `dialog` 动作报出打开中的 `alert` / `confirm` / `prompt` 并接受或
  拒掉它。对话框卡住页面时，命令超时的报错会点名这一点，不再看起来像连接已经死掉。

- **`/session` 有了子命令，其中三个用于删除会话**：`/session statistics` 就是原来不带参数的
  `/session` 打印的会话概要；`/session rm-current`、`/session rm-project`、`/session rm-all`
  分别删除当前会话、当前项目目录下的所有会话、所有项目目录下的所有会话。三者都先弹 Yes/No 确认框，
  写明会删掉多少个会话文件、在哪个目录，删完回报成功条数。`rm-current` 删完自动开新会话——否则
  下一次写入会把刚删掉的文件重新建出来。`/session <序号>` 的按序号切会话保持不变，子命令名在终端
  UI 的输入框里可补全。

- **桌面 GUI 中键复制并粘贴，与终端 UI 一致**：有选区（对话流或输入框）就复制进系统剪贴板并粘到
  光标处，没有选区就粘贴剪贴板内容。面板里没有可写的输入行，所以面板打开时只复制不粘贴。

- **桌面 GUI 输入框不再把折行行尾的那个字裁掉一半**：折行宽度取的是输入卡片宽度减去左右内边距，
  而卡片左右还有 1px 边框，于是每个视觉行都多算了 2px 宽，行尾那个字被 flex 压掉一半才换行。现在
  边框也一并扣掉。

- **新增 `sandbox` 扩展，把 shell 命令关进 OS 级边界**：`permissions` 是执行前的判定，动态拼出来
  的命令只能靠猜；这个扩展把文件系统规则交给内核。启用时探测后端（Linux `bwrap`、macOS
  `sandbox-exec`）并**真的用它起一个 `true` 进程**——bubblewrap 在禁用用户命名空间的容器里装得下
  却起不来，只有到 spawn 才暴露。此后每次 `bash` 调用都在「整棵文件系统只读 + 当前工作目录与系统
  临时目录可写 + 网络放行」下执行：`~/.ssh` 够不到了，`~/.cargo` 也一起变成不可写，要让它编译就把
  缓存目录指到工作目录里。探测失败**不会**退回无沙箱执行，而是装上一个「一律拒绝启动」的包装器并
  附上原因，失败因此是看得见的；要恢复无沙箱执行，显式在 `/extension` 里关掉本扩展。`/sandbox`
  打印当前后端、可写路径与网络状态。默认不启用。

- **`/debug receipts` 回答「这个会话到底做了什么」**：它逐行读会话里本就留下的记录——每次调用的
  `tool_started`、给退出码与 `edit` 精确增删行数（来自 patch）的 `toolResult` 消息、回合级失败的
  `operation_finished`——再读 `permissions` 的 review log 补上**根本没跑起来**的那些调用
  （`deny` / `ask`），后者在会话里不留痕迹。回执先给汇总（改了哪些文件、跑了多少命令、MCP 调用、
  子代理、失败、审批），再按时间序列出本轮经过；`/debug receipts <n>` 抬高明细行上限。解析是逐行的：
  命令 busy-safe，正在跑的这一回合写了一半的尾行被跳过，而不是让整份回执失败。

- **`/debug settings-key` 会点名 prux 不认识的设置键**：能解析、但键名打错的 `settings.json`
  （`"compactReserveToken"`、`"theme_gui"`）原本被静默忽略——那个值从来没生效过。新增的子命令
  会列出每个未知的顶层键与它的 `文件:行号`，有已知键的编辑距离在 2 以内时给出 `did you mean "…"?`；
  没有问题时只回一行确认。只存在于全局文件的键（`theme`、`skills` 等）写在 `.prux/settings.json`
  里同样会被点名——项目层只认 `defaultTools` / `disabledExtensions` / `enabledExtensions`。
  语法错误仍归启动警告；每次执行都重读这两个文件。

- **新增 `pr` 扩展：`/pr run` 时把改动交付成一个 PR**：`vet` 只证明本机检查过了，`pr` 补齐
  交付那一串机械动作——建分支、提交、推、开 PR、等 CI 与评审，然后把结论贴回聊天区。**没有
  结算边界钩子，也不把失败喂回模型**：没人替你决定要不要推，坏掉的一轮不会自己变成 PR。失败的
  检查（`名称 — 结论`）与 `CHANGES_REQUESTED` 的评审意见连同 PR 链接一起列出，修完再跑一次
  `/pr run`——改动落在同一条分支上，同一个 PR 被更新。停止条件：全绿 / 有阻塞 / 等满
  `timeoutSecs` 仍无结论（pending 不猜结果），且**永不 merge**。当前就在基线分支时先建
  `prux/<日期>-<时分>` 分支再干活，已在 `prux/` 分支上则续在同一条；工作区干净且不在 `prux/`
  分支上什么也不做。不猜远端（没 `origin`、`gh` 没登录、探测不出默认分支都停下报原因）。配置在
  `agent_dir()/extensions/pr.json`，**没有项目级覆盖**——能提交进仓库的配置不该有权改你的推送
  目标。命令 `/pr status|run`。默认不启用，仅 `Dev` 模式：它会写 git 历史并推远端。

- **记忆条目现在需要证据，并有维护轮做 reconcile**：抽取时要求每条附上支撑它的具体证据——失败的
  命令与报错、文件与符号，或用户自己的原话——答成空串的条目直接丢弃。证据随条目存档并供维护轮
  判定，但**不进注入块**，注入内容因此仍然紧凑且稳定。每成功抽取 `maintainEvery`（缺省 4）次，
  额外一次请求把已存条目 reconcile 一遍，要求每个 id 恰好一个决定：`keep`、`adapt`（改写含糊或
  把两件事挤在一句里的条目，可下调置信度）、`retire`（不再成立，或已被另一条完全覆盖）。缺 id、
  重复 id、未知 id、`adapt` 没给正文——任何一条不满足就整批失败、一个字节都不改（半套决定会让
  记忆图处在谁也说不清的状态）。`/memory maintain` 可按需触发一轮；旧存储没有 `evidence` 字段
  照样能读。

### 变更

- **`/pr run` 接手已提交但没推的改动，提交信息改由模型写**：工作区干净、但本地有远端没有的提交
  时，原本什么也不做——「已经 commit 了」并不等于交付完成，现在照样推上去开 PR（直接提交在基线
  分支上的仍不动，那等于往 `main` 上推）。有未提交改动时不再往历史里塞一条 `prux: <会话标题>`
  （标题是用户的第一句话，常常和这批改动对不上），而是把暂存的改动摘要交给当前模型写一条
  subject + 可选 body；没选模型、请求失败、输出为空时才回落会话标题——写不出信息不该让交付停在
  本地。请求带上当前会话 id（`x-opencode-session`）：缺了它 opencode 系 provider 会直接回
  `400 MissingSessionID`。请求的系统提示词就用**当前会话那一份**（用户写在里面的提交约定照生效），
  只在用户消息里补一句「只回提交信息，不要跑检查 / 测试 / 构建」。PR 的标题与正文同理：不再拿会话
  标题当标题、也不再加固定的一行「Delivered by prux」，而是把整条分支相对基线的改动摘要交给模型
  写一份标题 + Markdown 正文（同一次交付里两次请求，都带会话 id）。PR 那份用**自己的**系统
  提示词，不拿会话的：会话提示词里写着工具协议与工具清单，而一次性请求没有 `tools` 参数，模型
  会把工具调用块当普通文本吐出来，PR 标题就变成一堆 `<｜DSML｜ invoke …>`（这种输出现在直接当作失败
  回落）。PR 这份是 **JSON 输出**（`{"title", "body"}`）：按行切分一旦模型多写一句前言就会
  把半句话当成标题，而对象要么解析成功、要么回落到会话标题，没有中间地带。

- **新增 `/pr new`：每次都开一条全新的 PR**：`/pr run` 会把同一批改动续在已有 PR 上，
  想为另一次改动单独开一条 PR 时没有去处。`/pr new` 走同一条交付路，但不查已有 PR、也不续用
  当前交付分支——总是新建一条 `prux/<日期>-<时分>` 分支再推，因此拿到的是全新的 PR（GitHub
  也不允许同一个 head 分支挂两条开着的 PR）。工作区干净且没有未推送提交时它什么也不做，免得拿
  同一批提交再开一条内容重复的 PR。

- **隔离子代理不再从冷目录起步，而是直接复用仓库里已有的编译产物**：`Agent({ isolation: "worktree" })`
  建出的是一份干净的 worktree，于是第一次 `cargo build` 要把所有依赖重编一遍。现在会把主仓库里被
  gitignore 的产物（`target/`、`node_modules/` …）以 reflink 方式复制进副本——文件系统支持时
  （btrfs、XFS、APFS）副本与原目录共享磁盘块，代价是毫秒级而不是几个 GB。只尝试 reflink：
  不支持的文件系统（ext4、NTFS、tmpfs）或跨文件系统的复制会被整体跳过，而不是退化成一次比不拷
  更慢的全量拷贝。复制过来的都是被忽略的文件，因此不会混进隔离运行留下的那个分支。

## 修复

- **分支上的 PR 已经关了时，`/pr run` 另开一个新的**：`gh pr view <分支>` 连已关闭 / 已合并
  的 PR 也会返回，于是同一条分支上再跑一次 `/pr run` 会复用它——推上去的提交没有去处，还得
  到一句“旧 PR 全绿”。现在只复用开着的 PR：关着的那条在聊天区说一声，然后另开一个新的，
  等 CI 也改成按 PR 号读状态，不会读到旧的那条。

- **`@` 补全面板只在真有候选时才出现**：文件候选由后台扫描得出，于是敲 `@`（或敲一个什么都
  匹配不上的字符）会先弹出只有输入框、一行候选都没有的面板，等扫描回来是空才收起，输入卡
  每敲一下都跳。现在面板由扫描结果创建：没有条目就没有面板，与 `/` 一致；过期结果也不会
  把 Esc 关掉的面板重新弹回来。

- **桌面 GUI 的扩展面板只在内容变化时重绘**：打开着的 `/btw`、`/doctor` 等扩展覆盖层原本每秒
  重拉 10 次视图、每次都请求新帧，什么都没动也在刷。现在只有 GUI 实际渲染的字段（标题、抬头、
  正文、脚注、选中项、输入焦点或输入缓冲）与上一帧不同才算变化，面板静止时不再重绘。

- **桌面 GUI 窗口在 Wayland 下是不透明的**：它要求客户端绘制装饰，而窗口层把
  `decorations == Client` 当作透明窗口——surface 带着预乘 alpha 交给合成器、也没有 opaque
  region，整窗重绘时会闪。现在装饰交给服务端，surface 声明为不透明并带上 opaque region。

- **桌面 GUI 会话行右键菜单同时只高亮一项**：键盘选中与鼠标悬停原本是两套独立高亮，悬停 `Delete`
  时 `Rename` 还亮着，菜单里两行同时高亮。现在悬停移动的就是那一份选中项（与方向键一致），文字
  颜色随之变化。

- **删掉工程的最后一个会话不再留下空文件夹**：每个工程的会话都放在 `sessions/` 下的独立目录里，
  `/session rm-current`、`rm-project`、`rm-all` 与桌面 GUI 侧栏的删除都只删文件、留着目录。现在
  最后一个会话走的时候目录一起走（`/new` 与 GUI 的 `+` 也不再预先建目录），目录列表与实际内容一致。

- **桌面 GUI 输入框排满一行时，行尾光标不再把字顶出卡片**：那个光标方块原本追加在已经排满的行
  末尾，于是压到了卡片边框外面。现在当前行没空位时它自己占下一视觉行；每个字符的宽度也向上取整
  到整数物理像素——布局引擎按整数像素摆放逐字符元素，亚像素余量会让每行多塞一个字。卡片改为包
  一层再交给测量，量到的是卡片本身而不是卡片内的输入行。

- **桌面 GUI 输入框能补全 `/session` 的子命令**：以前只有扩展命令能声明子命令，`/session` 什么
  都不提示。现在列出内置的 `statistics`、`rm-current`、`rm-project`、`rm-all`，且与命令分发一致，
  同名扩展命令抢不走它们。

- **桌面 GUI 的 `/history` 面板用满整行宽度**：历史 prompt 没有行尾说明，却仍按「主文案截在说明
  列宽 + 行尾说明」的版式排，于是每条都只占面板四分之一就被截断，右边留一大片空白。现在行尾说明
  为空的行走「主文案独占整行」；带说明的 `/命令`、`@路径` 候选仍是说明紧跟主文案。

- **桌面 GUI 只渲染看得见的那部分对话**：长会话以前每次重绘都重建全部对话块，单帧要几十毫秒，
  只要有内容在动整窗就会闪。现在离视口一屏以外的块只放一个按实测高度定的占位块，重绘只为屏幕
  上的内容付钱；这些块的行登记从上一帧回放，选区与复制仍能拿到全文。屏幕外发生变化了的块要等
  它滚回视口才更新高度。
- **桌面 GUI 不再每帧把自己重绘一遍**：对话流的测量与钉底回调拿布局结果做精确浮点比较，只要不
  相等就请求下一帧——长会话里累加出的亚像素误差永远对不上，于是整窗持续重绘，静止不动也一直闪。
  现在这些比较允许半像素误差，渲染每行时文本与高亮也只构造一次。设 `PRUX_GUI_FRAMES=1` 可以让
  GUI 把帧间隔报到 stderr，静止时还在刷就说明还有东西在请求重绘。
- **桌面 GUI 不再每帧重解析覆盖层面板的 Markdown**：`/changelog` 这类 Markdown 面板以前每次重绘
  都把整份正文重新喂给解析器与语法高亮，滚动长 changelog 会卡顿。现在解析结果缓存复用，直到正文、
  明暗主题或排版宽度变化才重做（助手正文早就是这套做法）。
- **新会话落盘后会把文件路径补发给 UI**：`/new` 起的是延迟落盘会话，切换回执里没有路径，
  而之后也没有任何地方补发——于是在下一次切会话之前，`/fork` 一律回 `no current session`，
  `/resume` 不把当前会话标成当前，扩展也把会话记到 `default` 键上。现在首条消息写出会话文件时
  worker 就把路径报给 UI，并带着它重发一次会话切换给扩展。
- **桌面 GUI 能渲染表格、公式与 mermaid 图了**：助手正文里的表格以前被挤成一段连排的文字，
  `$...$` / `$$...$$` 原样显示成源码，`mermaid` 围栏只当普通代码块。现在表格按真网格渲染
  （列宽按该列最长单元格分配、单元格自动折行），行内公式与块级公式都走与终端 UI 相同的
  LaTeX → Unicode 渲染，`mermaid` 块也渲染成同样的盒图。LaTeX 与 mermaid 的渲染器随之搬到
  core 层，两端共用一份实现。

- **桌面 GUI 的状态圆点与文字视觉中心对齐**：品牌行里 `prux` 旁那个圆点（以及工具卡片的状态点）
  以前居中于行盒，比字母的视觉中心高约 2px，看起来偏上；现在补上这段偏移，圆点与文字在同一条
  水平中心线上。

- **桌面 GUI 的斜杠命令不再吞掉参数**：`/compact <指令>` 与 `/bug <提示>` 以前被当成未知命令丢弃，
  `/history` 把文本塞回输入框而不重新弹搜索面板，`/name <名称>` 改名后侧栏还显示旧标题。

- **桌面 GUI 的 `/rewind` 生效**：以前命令直接发给 agent，会话分支确实回退了，但对话区还挂着那条
  输入和它之后的全部输出。现在走与终端 UI 相同的确认面板（回退并把输入还回输入框 / 回退并丢弃 /
  取消），回退后按裁剪过的历史重建对话区。

- **桌面 GUI 的图片保持原始比例**：`@图片` 引用与工具返回的图片以前固定塞进一个方块，宽截图上
  下会留两条空白。现在按图片自身尺寸算：小图用原尺寸，大图等比缩到 360px 上限、不拉伸，解码器
  量不出尺寸的格式退回原来的方块。

- **桌面 GUI 的图片定在发送那一刻**：`@图片` 原来每帧都去路径重读，同名文件一改（再存一次截图），
  已经发出去的那条消息的缩略图就跟着换。现在每条用户消息在发送时按内容拍一份快照，缩略图改按文件
  字节建图片源，缓存键跟着内容走而不是路径；临时文件也改成按整份内容的哈希命名——原来的「长度 +
  首字节」会在不同图片之间撞名，撞上就一直显示旧图。

- **桌面 GUI 的状态栏在回合中间也走字**：用量以前只随回合结束的快照更新，流式与长时间的工具调用
  期间上下文占用、缓存命中率、in/out 与费用一直停着——新会话整轮读到的都是 `0% · CH 0%`。现在
  按 agent 事件随每条消息落地推进：assistant 的 usage 作为上下文锚点，其后的消息与工具结果在锚点上
  估算，压缩后按压缩事件给出的规模重置锚点。忙碌时消息数仍省掉，行首已经是 `Working…`。

- **`Ctrl-W` 不再吞掉 `Ctrl-Shift-W`**：删除前一个词的快捷键以前匹配任何 `Ctrl-W`，于是输入框把
  新的工作目录快捷键吃掉了；现在要求 shift 未按下。

- **桌面 GUI 的文件修改按 diff 渲染**：`edit` 卡片头部显示文件路径，改动本身按 diff 画——右对齐的
  行号列、新增行用成功色、删除行用错误色，`@@` 块头与上下文行用弱色，单行替换还会按词标出变动
  片段。diff 取自工具结果（`details.diff`），还没有结果时用调用阶段算出的预览，所以编辑过程中卡片
  就有内容。`write` 卡片同样只把路径放在头部，不再把整份文件内容塞进折叠的参数摘要，正文则按
  终端 UI 的样子折叠预览前几行（可点开看全部）。

- **桌面 GUI 从空白会话开始也跟随新内容了**：以前只有「贴过一次底」之后才会钉住滚动，而内容不足
  一屏时目标偏移是正数、被判成「用户已滚开」；于是全新会话的第一轮输出一路长到视口下面，得手动
  滚一次到底才开始跟。现在钉底一开始就是接管的，目标偏移按真实滚动范围夹取，第一轮就贴底，只有
  真的往上滚才停止跟随。

- **桌面 GUI 的 `/` 面板列全了能执行的命令**：`hotkeys`、`session`、`changelog`、`trust`、`bug`、
  `theme`、`reload`、`settings`、`extension`、`skills`、`scoped-models`、`login`、`logout`、
  `usage`、`debug` 以前能执行但不在候选里。`/name <名字>`、`/thinking <级别>`、`/model <id>`、
  `/session <序号>`、`/export <路径>`、`/import <路径>` 现在会吃掉参数而不是被当成未知命令
  （`/model` 有 scope 时只在 scope 内匹配，`/session` 按会话目录的序号切换——顺序与侧栏一致，
  最近修改在前——越界时提示可用范围），`/theme` 与 `/hotkeys` 分别是外观选择器与快捷键页的别名。`/` 面板里没有候选时按 Enter
  会把已输入的内容补全执行，而不是什么都不做——之前带参数的命令就卡在「No matches」上。`Ctrl-M` 命令面板只保留 `/` 面板没有的功能，外加有独立面板的 `skills`、
  `extension`、`settings` 三个，同一个动作不再占两个入口。

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
- **桌面 GUI 的 `/cache-usage all` 不再卡住窗口**：以前取不到异步运行时就在视图线程上同步扫描，
  会话一多整个窗口就停住，面板要等扫完才出现。现在扫描放到后台，面板立刻弹出并显示
  `scanning sessions…`。
- **桌面 GUI 的信息面板能用方向键滚动了**：滚动偏移的夹取方向反了，`/usage`、`/help`、`/session`、
  `/changelog` 与扩展详情页里的 `↑` / `↓` / `PgUp` / `PgDn` 只会跳回顶部，`End` 也到不了底部；
  现在都按实际可滚范围移动。
- **桌面 GUI 的 `/rewind` 能用了**：它在 `/` 面板里列着却没有实现，敲完只会把命令文本塞回输入框；
  现在会撤销上一条用户输入。
- **桌面 GUI 的 `/debug` 有输出了**：以前列在 `/` 面板里但什么都不做；现在裸 `/debug` 会把诊断信息
  （provider、model、消息数、自动压缩与重试、工作目录、会话）写进对话流，`/debug info` / `provider` /
  `system-prompt` 子命令仍由 `debug` 扩展提供。
- **选中文字时行内样式不再盖掉选区底色**：选区底色与行内代码、链接的高亮在交界处切成互不重叠的
  段落，重叠的那些词不会再把选区底色吞掉，也不会只画出半个选区。
- **桌面 GUI 会忘掉已不存在的工作目录**：工作目录选择里的最近列表以前只进不出，删掉或挪走的项目
  一直留在菜单里点不开。现在启动时剔除已经不存在的路径并写回清单；切换工作目录时先落地新快照，
  再拿它的 `cwd` 和旧值比较，不会漏掉变化。
- **桌面 GUI 的 `bash` 卡片不再截断命令**：命令以前挤在头部当一行摘要（带 `command=` 前缀、会被
  截断、还和状态标签抢宽度），现在占卡片第二行、整行折行、用代码色，它占掉的行数从输出行数预算里扣除。
- **空助手块不再撑出空白**：只发起工具调用、正文和思考都为空的回合以前渲染成空块，流的 `gap` 仍按
  子节点数计算，相邻块之间凭空多出一段间距；现在渲染时跳过这类空块。
- **输入框为空时按 `Enter` 回到最新输出**：以前什么都没输入时按 `Enter` 毫无反应，视图停在上面下
  不来；现在会重新钉底。
- **工具卡片的「还有 N 行」只提示一次**：`bash` 卡片以前在命令下面、diff 或写入正文下面、输出下面
  各出一条提示，各自只算自己那段的溢出；现在把截掉的行数累加，卡片底部只出一条。
- **桌面 GUI 展开卡片不再抖动**：展开改变高度后，鼠标抬起时指针仍落在卡片上，会再命中一次把它切
  回去。同一条目现在 250ms 内只接受一次切换，切换时还会清掉上一帧的实测高度，布局不会压在旧高
  度上。
- **桌面 GUI 的 `@` 面板与终端 UI 用同一套文件搜索**：以前是对一份一次性列出（限深度、只列文件）
  的路径表做子串过滤，目录、更深的文件、隐藏文件都搜不到，顺序也不一样。现在走同一个后台目录遍历
  与同一套路径打分：目录也是候选（补全成 `dir/` 便于继续往下敲），`.gitignore` / `.ignore` /
  `.fdignore` 生效，隐藏文件可见，排序按分数再到层级、长度、名字，含空白或 CJK 标点的路径补全时
  会加引号。输入目录前缀就把遍历范围收到该目录（`@src/mo` 只扫 `src`），`(`、反引号或 CJK 标点
  之后的 `@` 会触发面板而 `user@example.com` 里的不会，继续输入会作废上一轮遍历。
- **桌面 GUI 的补全面板没条目就收起**：`/命令` 与 `@路径` 以前会在一个候选都不剩时留一个
  `No matches` 的空框，终端 UI 是直接关面板。现在命令、子命令、路径扫描无候选就立刻收起面板；面板里
  敲的字会先写回输入框再收起，所以 `Esc`、退格退到空、零匹配三种情况都不会让刚敲的字消失。唯一的例外
  是 `/history`：零匹配时面板留在原地当搜索框，提示文案改成 `No matching history`。`/` 前的行首缩进
  现在也会触发面板——与终端 UI 一致。

- **桌面 GUI 的 `/history` 边输入边列出历史 prompt**：敲 `/history`（或 `/history bug`）就在输入框
  上方弹出本项目的历史，最近优先，不必先按 Enter。查询按空白分词、大小写不敏感，每个词都要出现在
  整段 prompt 里（词序无关），正文在第二行以后的 prompt 也搜得到。Enter / Tab 把选中的 prompt 写回
  输入框（多行 prompt 就写回多行）但不发送；零匹配时面板留在原地，不会把 `/history …` 当 prompt 发
  出去。`@clear` / `@clear-all`（仅查询以 `@` 开头时现身）删除本项目的历史或所有项目的历史，删前弹
  Yes/Cancel 确认并给出待删条目数。

- **桌面 GUI 的补全面板不再因移动光标弹出**：`/命令` 与 `@路径` 以前每按一个键就重算，在一段
  没改动的文字中间按左右方向键也会把面板重新弹出来挡住内容。现在只有输入框文本真的变了才重算补全。

- **桌面 GUI 中断回合后真的停下来**：`Esc`、停止按钮与扩展发起的中断现在都会补发 `agent_end`
  （与终端 UI 一致），靠事件断电的扩展（如空闲超时看门狗）就不会以为回合还在跑、把已经停下的会话
  又拉起来；同时取消可取消的后台工作（如 MCP OAuth 登录）、把排队的 steer / follow-up 取回输入框。
  回合运行中 `Esc` 在菜单与覆盖层处理之前先走中断。

- **桌面 GUI 重启回合不再丢掉已排队的输入**：扩展重建 prompt 期间积压的 steer 消息现在与新 prompt
  一起发出，而不是被丢弃。

- **`browser` 扩展不再点穿遮罩**：`click` / `dblclick` 先检查元素中心点，被别的元素盖住时直接报错并
  点名盖住它的元素（如 `covered by <div#banner>`），元素 `disabled` 或没有可点面积时同样报错——以前
  照旧派发点击并回「已点击」。

- **桌面 GUI 的 mermaid 图与公式不再错位**：等宽内容以前交给系统 `monospace`，中文 fallback 的
  字宽与西文 advance 凑不成 2:1，含中文的行每过一个汉字就往左缩 0.2em，盒图竖线跟着歪。现在内嵌
  Source Code Pro 并把这类内容按终端网格排（半角一格 0.6em、汉字占两格居中），代码块与右侧文件
  面板也统一用这个字体。

- **桌面 GUI 表格不再把短列压成竖排**：列宽以前按字符数比，两个字的表头对上几十字的描述只剩一个字宽，
  「项目」被逼成一行一个字。现在按显示宽度（汉字算两列）比并夹在 8~40 列之间，短列放得下内容、
  超长列照旧折行。

- **桌面 GUI 的 markdown 链接可以打开**：`[文字](url)` 以前只是变色，点不动。现在 Ctrl+点击用系统
  默认浏览器打开（与终端 UI 同一手势、与右侧信息面板里的链接同一条路径），渲染区与覆盖层面板里的
  markdown 都生效；普通点击仍留给选字。

- **桌面 GUI 的 `Ctrl-N` 不再新建会话**：它与终端 UI 的命名会话过滤撞键，快捷键已移除，改用 `/new`
  或侧栏的 `+` 按钮。

- **桌面 GUI 的覆盖层与停靠面板长行会折行，不再戳出卡片**：每行原本是一排按样式片段切开的 flex
  子元素，而文本的自动最小宽度就是它不折行时的整行宽度——于是这一行永远缩不到卡片宽度，长文本
  （子代理的任务说明、停靠状态行）直接画到面板边框外面。现在片段允许收缩，这类长行在卡片内折行。

- **桌面 GUI 删除会话与终端 UI 一致，先进回收站**：侧栏的删除动作原本直接 unlink，点错一次就
  找不回来。现在先走 `trash` CLI，失败才回退 unlink；并且和终端 UI 一样不允许删除当前正开着的
  会话——那个文件仍在被追加写入。确认框也不再写「无法撤销」。

- **桌面 GUI 的 `Esc` 先关面板、再中止回合**：正在输出时 `Esc` 原本先命中忙碌态分支，于是想收起
  覆盖层或菜单，却把正在跑的回合取消了。忙碌态的中止现在排在覆盖层、下拉菜单、补全面板与搜索框
  之后，与终端 UI 的顺序一致。


- **提权面板的按钮改成「Submit」**：窄面板里「Submit answers」把按钮撑到和说明文字抢宽度，
  现在与「Cancel」等长（终端 UI 与桌面 GUI 共用同一份视图）。

- **桌面 GUI 侧栏底部的目录块不再画圆角**：它贴着窗口底边，圆角只在下半截露出一道缺口，
  与侧栏的直边不一致。

- **`leak-guard` 不再对路径与会话文件名误报，也不会反复提示**：高熵扫描把 `/` 也算作候选串的
  字符，于是任何路径都连成一整条「随机串」，UUID / 哈希 / `2026-11-02T10-25-54-666Z_<uuid>`
  这类会话文件名也照报不误。现在路径不再连成一个候选，机器生成的标识直接跳过，告警记录满了
  改为挤掉最早一条而不是整体清空——历史里这类名字一多，记录每轮都被清空，同一个提示就会
  每回合重弹一次。

- **`leak-guard` 的提示会指出到底哪一段触发了告警**：原先只说「像凭据（高熵）」，路径、base64
  串和哈希在提示里长得一模一样。现在请求前与工具调用前的提示都附上命中片段的首尾预览（如
  `high-entropy: q8Zr2NvT…Zx4Q`）；被拒绝的工具调用只报类别，因为那条原因会回到模型上下文。

- **桌面 GUI 新建的会话会出现在侧栏并高亮**：会话文件要等首条消息才写出，而侧栏原先只在切换
  工作目录或会话改名时才重读，于是新会话的行一直不出现，当前会话也就高亮不到。现在会话路径
  出现或变化都会重读列表。
