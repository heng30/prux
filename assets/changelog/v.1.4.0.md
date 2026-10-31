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

- **The desktop GUI queues what you send while a turn runs**: `Enter` puts the text into the
  steering queue, so the model reads it inside the same run (before its next step) instead of a
  fresh turn after the fact, and `Alt+Enter` queues a follow-up that is sent once the turn settles.
  Both queues drain by `steeringMode` / `followUpMode` as they do in the terminal, and the waiting
  messages are listed above the input box as `Steering:` / `Follow-up:` — they enter the
  transcript only when actually sent. `Alt+Up` pulls them all back into the input box (steers
  first, then follow-ups, separated by blank lines and kept ahead of the current draft) and empties
  both queues.

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

### Fixed

- **Images in the desktop GUI keep their aspect ratio**: `@image` references and pictures returned by
  a tool used to sit in a fixed square, so a wide screenshot left empty bars above and below. The
  thumbnail is now sized from the image itself — small pictures render at their own size, large ones
  scale down to the 360 px cap without distortion — and formats the decoder cannot measure fall back
  to the old square.

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

## 修复

- **桌面 GUI 的图片保持原始比例**：`@图片` 引用与工具返回的图片以前固定塞进一个方块，宽截图上
  下会留两条空白。现在按图片自身尺寸算：小图用原尺寸，大图等比缩到 360px 上限、不拉伸，解码器
  量不出尺寸的格式退回原来的方块。

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
