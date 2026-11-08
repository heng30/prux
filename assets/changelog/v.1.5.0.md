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

### Fixed

- **A `ref` on Linux was not re-checked before acting**: the macOS and Windows backends re-read the
  element and refuse when its name or role changed, but the AT-SPI path acted on the recorded
  `(bus, path)` directly, so a reused path could drive a different control. Every Linux action and
  read now verifies the identity first and fails with `[STALE_REF]`.
- **Chinese in monospace text fell back to a system font**: code blocks, inline code and the diff
  view asked for Source Code Pro only, which carries no CJK glyphs, so each Chinese character was
  drawn by whichever font the system picked — different on every machine and visibly unlike the
  rest of the UI. Monospace text now declares the embedded CJK font as its glyph fallback.

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

### 修复

- **Linux 上按 `ref` 操作前不复核节点身份**：macOS 与 Windows 后端会重读元素、名称或角色变了就拒绝，
  但 AT-SPI 那条路径直接拿记录的 `(bus, path)` 动手，路径被复用时会操作到别的控件。
  现在每个 Linux 动作与读值都先复核身份，对不上就报 `[STALE_REF]`。
- **等宽文本里的中文落到系统字体**：代码块、内联代码与 diff 视图只指定 Source Code Pro，它没有中文字形，
  于是每个汉字都由系统随手挑的字体画出来——各机器不一样，也和界面其余部分明显不搭。
  现在等宽文本声明内嵌中文字体作字形回退。

