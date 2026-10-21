# Prux 迁移指南：pi-permission-system → prux 扩展 `permissions`

> **来源**：`@gotgenes/pi-permission-system` v41.0.0（npm tarball，`packages/pi-permission-system`）
> **目标**：`src/extensions/permissions.rs`（门面 + 门控）+ `src/extensions/permissions/`
> **性质**：**Tier1 移植**。上游是 31.8k 行 / 172 个 TS 文件的完整策略引擎（含
> `web-tree-sitter` + `tree-sitter-bash` 的 AST 解析、zod schema、MCP/skill 细分表面、
> 跨扩展广播总线、子代理 ask 转发、内联阻塞式确认对话框），本仓只取**四层策略 + bash 静态分析**
> 这条主线，其余按下面的「明确不做」清单记档。
>
> **Tier2 追补**：确认面板已补上（§4.2），代价是核心新增了一个异步钩子与一条等待式提问通道
> （`BeforeToolCallAsync` + `ask_prompt`，见 §4.6）。
>
> **更新记录**
> - `Tier1 实施完成`：`src/extensions/permissions.rs` + `permissions/{config,policy,paths,bash,
>   review,command}.rs` 落地；`extensions.rs` 加 `pub mod permissions;` 与
>   `PRIORITY_PERMISSIONS = 84`。54 个扩展单测通过；`cargo test --lib` 全量 3811 通过。
>   核心**零改动**，唯一一处全局性调整是给 `serde_json` 打开 `preserve_order`
>   （理由见 §4.1）。
> - `Tier2 交互式 ask`：核心加 `ExtensionHook::BeforeToolCallAsync` + `Extension::before_tool_call_async`
>   （`prepare_tool_calls` 变 async，唯一生产调用点加 `.await`），`core::extensions::ui` 加
>   `ask_prompt` / `ask_pending` / `ui_ready` / `set_ui_ready`，事件循环加提问面板对账
>   （`Panel::active_ask_id` → `App::active_ask_id`）。`permissions` 改为声明异步钩子并在命中
>   `ask` 时弹面板。扩展单测 55 + 集成测试 8 通过。

---

## 〇、摘要

### 上游规模

| 维度 | 数值 |
|---|---|
| TS 源码 | 31 814 行 / 172 个文件 |
| 子树 | `access-intent/`（bash AST 分析，40+ 文件）、`policy/`、`config/`、`session/`、`handlers/`、`presentation/`、`authority/`、`exposure/`、`logging/`、`path/`、`tool-input/` |
| 依赖 | `web-tree-sitter` + `tree-sitter-bash`（bash 语法树）、`zod`（配置校验） |
| 文档 | `README.md`、`docs/configuration.md`（1 724 行）、`session-approvals.md`、`opencode-compatibility.md`、13 篇 `docs/migration/*` |

### 迁移原则

1. **语义对齐，实现重写**：四层名（`path` / `external_directory` / 工具名 / `bash`）、三态值
   （`allow` / `ask` / `deny`、带 `reason` 的对象）、`*` 跨分隔符与 `?` 单字符、
   last-match-wins、most-restrictive-wins、方向变体 `*_read` / `*_write`、`~` / `$HOME`
   展开——全部照上游语义实现，不逐行翻译。
2. **不引重型依赖**：上游用 tree-sitter 解析 bash。本仓保持 `opt-level = "s"` 单二进制，
   改为自研词法扫描（`bash.rs`），并把「看不透」显式表达出来（`indirect` / `complete`），
   由门控按 fail-closed 处理。**判错的方向必须是「多拦」，不是「放过」。**
3. **架构差异就地折算**（§4）：prux 没有阻塞式确认对话框，`ask` 折算成 fail-closed 拒绝 +
   会话放行，这是本次移植最大的语义偏离，已与仓库所有者确认。

---

## 一、Tier1 范围（已实现）

| 能力 | 落点 |
|---|---|
| 配置：全局 + 项目（项目受信门控）、深合浅覆盖 | `config.rs` |
| 四层策略 + 方向变体 + last-match-wins + most-restrictive-wins | `policy.rs` |
| 通配匹配（`*` 跨分隔符、`?`）、`~`/`$HOME`/`${HOME}` 展开、词法归一化、符号链接双拼写、CWD 边界 | `paths.rs` |
| bash：语句切分、引号/转义、`$(…)`/反引号/子 shell 递归、重定向目标、环境变量前缀剥离、字面 `cd` 追踪、绝对拼写补充、透明包装剥离、不透明包装识别、heredoc 剥离（引号分隔符不插值） | `bash.rs` |
| 门控：`before_tool_call_ext`、全 deny 工具从工具表摘除、拦截文案、fail-closed、会话放行、`yoloMode` | `permissions.rs` |
| review log（NDJSON，`/permissions log`） | `review.rs` |
| `/permissions show\|allow\|drop\|allowlist\|init\|log\|reset` | `command.rs` |
| `/extension` 设置面板：`permissionReviewLog` / `yoloMode` | `permissions.rs::settings` |

---

## 二、明确不做（以及为什么）

| 上游能力 | 决定 | 理由 |
|---|---|---|
| `tree-sitter-bash` AST 解析 | **不做**，改自研词法 | 依赖体积与构建复杂度与单二进制定位冲突；词法版把不确定性显式化（`indirect` / `complete = false`）后按 fail-closed 处理，安全方向一致 |
| 上游对话框的**交互细节**：`y`/`s`/`b`/`n`/`r` 热键、双击确认、IME 键位映射、`promptNotifications`、`promptMaxRows`、`Ctrl+O` 展开、拒绝理由编辑器 | **不做** | prux 的面板是**方向键 + Enter** 的选择列表（与 `/model`、`/extension` 同一套），不引入一套并行的热键状态机。确认语义本身已补上（见 §4.2） |
| `mcp` 表面的 server/tool 粒度、`skill` 表面 | **不做** | prux 的 MCP 是单个 `mcp` 代理工具（`action`/`tool` 在参数里），没有上游那种「每个 MCP 工具一个工具名」的形状；按工具名表面判定 + `path` 层兜住 `arguments.path` 以外的形态不在 Tier1 |
| `PRUX_RESOURCES` 之外的 per-agent frontmatter 策略（`docs/configuration.md#per-agent-overrides`） | **不做** | prux 没有 per-agent 策略分层；策略与放行表在会话/子代理之间共享 |
| 子代理 `ask` 转发与队列（`permissions:ui_prompt` / `permissions:decision` 广播总线） | **不做** | 依赖阻塞式对话框；prux 的子代理走同一个门控，判定即拒绝 |
| `authorizerChain`（外部 live-authority 决策链） | **不做** | 没有消费者；需要插件生态才成立 |
| `shellTools`（把别的工具当 shell 工具处理） | **不做** | Tier1 只认内置 `bash` / `powershell` |
| `commandEffects`（用户自定义「命令对路径的效果」） | **不做** | 内置一张小字典（`rm`/`cp`/`mv`/`tee`/`sed -i` 等视为写），不开放用户配置 |
| `piInfrastructureReadPaths`、`yoloMode` 之外的运行期开关（`doublePressToConfirm`、`forwardingTimeoutMs`、`promptMaxRows`、`promptFieldMaxWidth`、`reviewLogFieldMaxWidth`、`permissionDialogKeys`） | **不做** | 都属于对话框或日志字段宽度，跟着对话框一起去掉；`reviewLogFieldMaxWidth` 用固定 1000 字符截断替代 |
| 无效高优先级配置的「clamp 到 ask」 | **不做** | 上游的做法（把继承来的 `allow` 全钳成 `ask`）需要完整的配置来源追踪；Tier1 的做法是**无效规则值被跳过**（该表面少一条规则）+ 加载告警，语义等价于「少放行，不放宽」 |
| OpenCode 兼容层、`docs/migration/*` 系列契约 | **不做** | 上游自身的演进史 |

---

## 三、语义对照

| 上游 | prux | 备注 |
|---|---|---|
| `permission["*"]` 缺省 `ask` | 同 | `Policy::universal` 缺省 `Ask` |
| 表面值 `"allow"` 等价 `{"*": "allow"}` | 同 | `Surface::CatchAll` |
| 值 `{action, reason}` | 同 | 理由进拦截文案与 review log |
| last-match-wins | 同 | **依赖 JSON 对象书写顺序**，见 §4.1 |
| `*` 跨分隔符、`?` 单字符、不支持 `**`/`[...]` | 同 | `paths::wildcard_match` |
| `path` → `external_directory` → 工具 → `bash` 四层，most-restrictive-wins | 同 | `Decision::tighten` |
| 方向变体 `path_read`/`path_write`/`external_directory_read`/`external_directory_write` | 同 | 裸键视为两方向 |
| 路径按原文 + canonical（符号链接）双拼写匹配 | 同 | `paths::canonical_forms` |
| bash 路径参数按原文 + 绝对拼写匹配 | 同 | `Command::absolute_text` |
| 「所有模式都是 deny 的工具从工具表隐藏」 | 同 | `Extension::filter_tools` |
| bash 语句逐条求值、嵌套命令替换/子 shell 也求值 | 同 | `bash::Scan` |
| 引号 heredoc body 不插值 | 同 | `strip_heredocs` |
| 无引号 heredoc body 里的命令替换求值 | 同 | `Origin::HeredocBody` |
| 前导环境变量前缀剥离 | 同 | `strip_env_prefix` |
| 字面 `cd` 推进工作目录，非字面 `cd` 后不做绝对拼写 | 同 | `cd_effect` |
| 透明包装（`env`/`nohup`/`timeout`/`nice`/`command`/`exec`/…）剥离后求值 | 同 | `unwrap_transparent` |
| 不透明包装（`bash -c`/`eval`/`sudo`/`xargs`/`find -exec`/解释器）→ `ask` | 同 | `detect_wrapper` |
| 解析失败 → `ask` | 同 | `Scan::complete = false` |
| `ask` 弹对话框等待按键 | **改为 fail-closed 拒绝 + `/permissions allow`** | §4.2 |
| 会话批准分「一次 / 本会话 / 本会话双向」 | **只保留本会话（按 surface+pattern）** | 没有对话框就没有「一次」这一档 |
| 拒绝文案列出「谁在问、工具、规则、值」 | 同（单行汇总） | `denial_message` |

---

## 四、实施中的实际偏离与修正

### 4.1 `serde_json` 启用 `preserve_order`（全局改动）

last-match-wins 依赖**配置里的书写顺序**。`serde_json` 的 `Map` 默认是 `BTreeMap`，
解析后键按字典序排列——`{"*": "allow", "*.env": "deny"}` 与
`{"*.env": "deny", "*": "allow"}` 会变成同一个对象，策略语义直接反了。

因此 `Cargo.toml` 给 `serde_json` 加了 `features = ["preserve_order"]`（`Map` 变成
`IndexMap`，`indexmap` 本来就在依赖树里）。

连带影响一处既有代码：`src/extensions/usage/fetch/kimi.rs` 的 `usages` 映射解析原先
**隐式依赖** `BTreeMap` 的键序（测试注释里写着「按键名排序」）。已改为显式
`entries.sort_by(...)`，让行为不再取决于 `Map` 的内部表示——这是修掉了一个隐式依赖，
不是功能变更。

### 4.2 `ask` → 确认面板（Tier2）

`Extension::before_tool_call_ext` 是同步的，返回值只能表达「放行 / 拦截 / 改写参数」，
而上游的 `ask` 要「等到用户按键」。Tier1 先折算成 fail-closed 拦截 + `/permissions allow`；
Tier2 补上了真正的交互，路径是给核心加一条**异步**钩子：

| 层 | 内容 |
|---|---|
| `ExtensionHook::BeforeToolCallAsync` | 声明它的扩展走异步门控，可 `await`；未声明者完全不变（不构造 owned 参数） |
| `Extension::before_tool_call_async` | owned 参数 + `BoxFuture`，默认桥接 `before_tool_call_ext` |
| `core::extensions::ask_prompt` | 入队 `Select` 面板 + `oneshot` 等答案，可选超时；无事件循环时立即 `None` |
| `ask_pending` / `ui_ready` | 前者避免顶掉别人的面板，后者表示「有没有人在看面板」 |
| `Panel::active_ask_id` + 每帧对账 | 面板被替换/关闭时给等待方回传 `None`（`Panel::close` 本身不回调） |

`permissions` 的判定因此变成：

- `allow` / `deny` → 原样；
- `ask` + `yoloMode` → `allow`（不问）；
- `ask` + 会话放行表已覆盖 → `allow`；
- `ask` + 无 UI / 已有别的提问在等 → `deny`（fail-closed，不弹面板避免顶掉别人）；
- `ask` → 弹面板：`Approve once` → 放行本次；`Approve for this session` → 写会话放行表并放行；
  `Deny` / `Esc` / 超时（10 分钟）→ 拒绝。

拒绝文案里的指引随情形变化：有 UI 时说「面板没有作答（取消/超时/已有别的确认）」，
无 UI 时说「本次运行没有可交互界面」；两种情况都给 `/permissions allow` 的复制粘贴串。

**与原方案的差别**：不再需要「被拦 → 敲命令 → 让模型重试」这一圈，代价是核心多了一个钩子
与一条等待通道（§4.6）。会话放行仍然**不覆盖** `deny`。

### 4.3 bash 绝对拼写放宽到裸词

上游只对「路径形」参数（含 `/`、`.` 开头、`~` 开头等，以及磁盘上真实存在的裸文件名）做
绝对化。Tier1 放宽到**所有非选项、非通配、非变量的词**（第 0 个词是命令名，不替换）：
`rm build` 因此也能匹配 `rm /repo/build`。多出来的候选串只会让判定更严格，
不会放宽——方向与 fail-closed 一致。

### 4.4 bash 普通参数的写方向靠命令名字典

上游用 `commandEffects` / `sed-invocation` / `awk-invocation` 等 40 多个文件推导「这条命令
对路径做了什么」。Tier1 用一张小字典：重定向目标按自身方向，`rm`/`cp`/`mv`/`tee`/`sed`/
`perl`/`git`/`npm` 等命令的路径参数视为写，其余视为读。因此
`path_write: {"*": "deny"}` 对 `cat a.txt` 不会误拦，但 `python - <<EOF` 这类看不透的
写法会落到「不透明包装 → ask」，不会被悄悄放行。

### 4.5 配置告警只提示一次

无效规则值、被跳过的未受信项目配置会经一条系统消息告警，并用进程级 `AtomicBool` 保证
只提示一次（否则每次工具调用都会刷屏）。`/permissions show` 每次都完整列出告警。

---

### 4.6 核心新增的接缝（Tier2）

| 改动 | 位置 | 为什么必须动核心 |
|---|---|---|
| `ExtensionHook::BeforeToolCallAsync` + `Extension::before_tool_call_async` | `core/extensions/hooks.rs`、`core/extensions.rs` | trait 上原本没有任何 await 点，扩展不可能「等一下再返回」 |
| `prepare_tool_calls` → `async fn` | `core/agent_session.rs` | 唯一生产调用点（`execute_tool_calls`）本就是 async，加一个 `.await`；测试点补 `block_on` |
| `ask_prompt` / `ask_pending` / `ui_ready` / `set_ui_ready` | `core/extensions/ui.rs` | 等待表 + `oneshot`；命中等待者时 `submit_ui_choice` 返回 `None`（不触发 follow-up） |
| `Panel::active_ask_id` + `App::active_ask_id` + `reconcile_ask_panel` | `modes/interactive/{panel,app,interactive}.rs` | `Panel::close` 不回调扩展，不补对账则等待方只能等超时 |

关于「阻塞会不会死锁」：`#[tokio::main]` 是 multi-thread runtime，agent 跑在 worker 线程、
UI 事件循环跑在 main 线程，阻塞 worker 不会冻结 UI。本轮仍选了异步路径，原因是**可取消**
（Esc 中断会 drop future）与**不占线程**，而不是因为阻塞不可行。

## 五、测试覆盖

`src/extensions/permissions/` 下模块内单测（54 个），覆盖：

- 通配：跨分隔符、`?`、回溯、last-match-wins 的两种书写顺序；
- 规则值：字符串/对象/非法值；缺省 `ask`；非法值被跳过且不放宽；
- 优先级：`path` deny 压过工具 allow、工具 deny 压过 `path` allow、方向表组合；
- bash：切分、引号、命令替换、反引号、子 shell、重定向、环境变量前缀、`cd` 追踪、
  非字面 `cd`、间接包装、透明包装剥离、heredoc（引号/未引号）、未闭合引号；
- 门控：`.env` 拦截与 `.env.example` 放行、bash 链逐条、重定向目标、
  `external_directory` 内外判定、无命中时的通用兜底、拦截文案里的放行指令。

Tier2 追加：

- `core::extensions::ui`：`ask_prompt` 经 `submit_ui_choice` 拿到答案且不触发 follow-up、
  取消（`None`）、超时、无 UI 时立即返回；
- `tests/permissions.rs`：有 UI 时弹面板 → `Approve once` 放行、`Approve for this session`
  写会话放行表且第二次不再拦、`Esc` 拒绝。这三个用例用进程级 `PRUX_AGENT_DIR` + 一把测试锁，
  因为门控跑在 spawn 出去的 runtime 任务里，读不到线程本地的 `AgentDirGuard`。
- 其余用例（无面板场景）统一在测试锁内 `set_ui_ready(false)`，断言 fail-closed 行为。

命令 handler 与 `App` 的耦合（`/permissions show` 的排版等）仍留给手工验证。
