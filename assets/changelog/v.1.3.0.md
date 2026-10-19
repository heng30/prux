## English

This release aligns prux with upstream pi 1.3.0.

### Added

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

### Changed

- **`read` shows its line range as `:start-end`** in the transcript and in HTML exports, the same
  shape the session viewer uses. Numeric strings and `null` are accepted, so `offset: "5"` now shows
  a range instead of nothing. `read`'s `offset` is documented as 1-based.
- **Switching models no longer feeds the previous model's reasoning to the new model.** Thinking
  blocks produced by a different provider, API or model are dropped instead of being sent as plain
  text, so the new model no longer treats another model's reasoning as part of its own reply.
- **codemode's tool description** now states that a tool declaring an output schema resolves with
  its value even when the call fails, instead of claiming every failure rejects.

### Fixed

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
- **`settings.json` now carries a `$schema` reference** (`https://pi.dev/schemas/settings.schema.json`)
  for editor completion and validation, and the packaged themes point their `$schema` at
  `https://pi.dev/schemas/theme.schema.json`. prux never reads the key; a value you set yourself is kept.

## 中文

本版对齐上游 pi 1.3.0。

### 新增

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

### 变更

- **`read` 的行范围标签统一为 `:start-end`**：对话区与 HTML 导出都按这个形状显示（与 session
  视图一致）。数字字符串与 `null` 也能解析，`offset: "5"` 不再什么都不显示；`read` 的
  `offset` 文档改为 1 起。
- **切换模型不再把上一个模型的推理喂给新模型**：其它 provider / API / 模型产生的 thinking 块被丢弃，
  而不是当普通文本发出去，新模型不会再把他人的推理当成自己回答的一部分。
- **codemode 的工具描述**改为说明「声明了 output schema 的工具失败时返回自己的值」，
  不再声称所有失败都抛错。

### 修复

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
- **`settings.json` 会写入 `$schema` 引用**（`https://pi.dev/schemas/settings.schema.json`）供编辑器补全与校验，
  内置主题的 `$schema` 也指向 `https://pi.dev/schemas/theme.schema.json`。prux 自身不读取该键，
  你自己改过的值会保留。
