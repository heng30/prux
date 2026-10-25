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
  Off by default and available in every extension mode; turn it on in `/extension`.

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

### Fixed

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
  会被拒绝。默认不启用（所有扩展模式可用），在 `/extension` 面板开启。

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

### 修复

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
  永远是非激活态；分支既没加过也没删过的已注册工具现在默认激活。
- **HTML 导出的 CJK 强调正常了**：CJK 标点紧邻 `**` / `*` 时不再显示字面星号。
