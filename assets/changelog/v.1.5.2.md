## English

### Added

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

### Changed

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

### Fixed

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
- **Exported HTML did not show the system prompt and tools**: `/export` and sharing wrote the entries
  only, so the two sections that explain what the session ran with stayed empty. Both now carry the
  system prompt and the tool declarations actually sent for this request (hidden ones excluded).
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

## 中文

### 新增

- **`/debug context` 说明上下文窗口被什么占着**：状态栏只给一个数——供应商上次实测的 token——
  而在第一轮落地 usage 之前它什么都不说，可那时系统提示词与工具 schema 已经占了几万 token。
  `/debug context` 改成按来源拆开：系统提示词（`base`、项目上下文、技能、MCP 段分别计数）、
  本次请求真正声明的工具 schema（名字 + 描述 + 参数 JSON，被隐藏的不算）、以及会话本身。
  有实测 usage 时已知部分按词表数出来、差额归会话；第一轮之前三块都是估算、总额取三块之和，
  因此不会再显示 0。窗口剩下的部分打成 `free`。属于 `debug` 扩展，仍然默认不启用（Dev 模式）。
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

### 变更

- **新会话默认用各 provider 当前的主力模型**：内置默认模型表改指目录里标为当前的那批——`anthropic` →
  `claude-opus-5-5`，`openai` / `azure` / `github-copilot` → `gpt-6.1-sol`，`google` →
  `gemini-4-argon`，`moonshotai` / `moonshotai-cn` / `opencode` → `kimi-k2.7-code`，`opencode-go`
  → `kimi-k3.1-pro`，`minimax` / `minimax-cn` → `MiniMax-M3`，`baseten` → `zai-org/GLM-5.3`，
  `vercel-ai-gateway` → `zai/glm-5.3`，`huggingface` → `moonshotai/Kimi-K2.7-Code`，
  `qwen-token-plan` / `qwen-token-plan-cn` → `qwen3.8-max`。改的只是「什么都没选」时的回退；
  `settings.json` 里存过的默认值或 `--model` 参数仍然优先。
- **新增 `qwen-token-plan-individual-cn` provider**：中国端点上的 Token Plan Individual 白名单，
  凭据为 `QWEN_TOKEN_PLAN_CN_API_KEY`。

### 修复

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
- **导出的 HTML 看不到系统提示词与工具**：`/export` 与分享只写会话条目，解释这次会话用什么跑起来的
  两节一直是空的。现在两者都带上系统提示词与本次真正声明的工具（被隐藏的不算）。
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

