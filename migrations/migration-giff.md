# Prux 迁移指南：giff → prux 只读审阅视图 `/giff`

> **来源**：`giff` v1.2.0（`https://github.com/bahdotsh/giff`，Rust 写的终端 git diff 浏览器；
> 本仓的本地只读检出是 `tmp/giff -> /home/blue/wayshot-fork/giff`）。
> 规模：`src/` 11 个文件、3 994 行；依赖 `ratatui 0.30`（`all-widgets`）、`crossterm 0.28.1`、
> `syntect 5.3` + `two-face 0.5.1`、`clap`、`toml`、`regex`，`edition 2021`。
>
> **目标**：`src/modes/interactive/giff/`（视图本体）+ `src/modes/interactive/` 下 6 个接入点。
> 本档与 `migration-subagents.md` / `migration-tasks.md`（上游插件整体移植）不同：
> giff **不是 pi 生态插件**，它自带渲染与事件循环，所以本文档记的是
> 「一个独立 TUI 应用被**收编**成 prux 的一个全屏视图」时的裁剪、重映射与接缝。
>
> **性质**：**先冻结范围**（只 review、不写工作树），再落地；本档按落地后的实际状态回填，
> 决策与未做项集中在 §6 / §7。
>
> **与既有迁移文档的关系**：与 `migration-v*.md`（pi 版本对齐）无关；与 `migration-kiss-*`
> / `migration-jcode-*`（同源 Rust 实现的功能差集）不同，本档的上游是**异构实现**，
> 因此「偏离」一章记的是宿主机制差异，而不是 pi 契约差异。

---

## 〇、摘要

### 结论

只移植 giff 的 **`review` 部分**：浏览工作树未提交改动（文件列表 + 双栏/统一 diff +
语法高亮 + 滚动 + 帮助）。上游的 rebase 体系（逐块接受/拒绝、写回工作树、`git commit`、
`--auto-rebase`、upstream 检测）**整块不做**——这个视图是只读的，跑完不留下任何痕迹。

落地后的形态：

| 维度 | 上游 giff | prux `/giff` |
|---|---|---|
| 入口 | `giff [from] [to] [-d args] [--theme] [--auto-rebase]` | 内置斜杠命令 `/giff`（无参数） |
| 生命周期 | 自己 `enable_raw_mode` + `EnterAlternateScreen` + 阻塞事件循环 | prux 主循环的一帧分支（无终端切换） |
| 状态存放 | 进程内 `App` | `App.giff: Option<GiffView>` |
| 退出后 | 退出即丢失 | `App.giff_restore` 记住文件与滚动位置，下次 `/giff` 恢复 |
| 主题 | 自带 dark/light + `~/.config/giff/config.toml` | 只读当前 prux 主题（`GiffStyle` 取色快照） |
| 语法 | `two_face` 的 `extra_newlines`（130+ 语言） | prux `render::syntax_hl`（默认集 ~75 + 内置 Zig） |
| 输入 | 无（纯浏览） | `i` 调出主页输入框，`Esc` 退回导航，`Enter` 提交并退出视图 |
| 代码量 | 3 994 行 | 1 726 行（5 个文件，含 18 个单测） |

### 路线图

| Tier | 主题 | 内容 | 状态 |
|---|---|---|---|
| **Tier1** | 数据 | 只读 `git diff` 采集 + 解析（文件切分 / hunk 行号 / 元数据与 `\ No newline` 丢弃） | ✅ 已实施（`giff/diff.rs`） |
| **Tier2** | 视图 | 状态机（焦点 / 视图模式 / 每文件滚动）+ header / 文件列表 / 双栏 / 统一 / 帮助模态 / 键位行 | ✅ 已实施（`giff/{mod,types,render}.rs`） |
| **Tier3** | 接入 | `App` 字段与渲染/按键/鼠标分流 + `/giff` 内置命令 | ✅ 已实施（§4） |
| **Tier4** | 宿主化 | 主题映射、语法高亮换用 prux 实现、`i`/`Esc` 输入态、退出恢复位置 | ✅ 已实施 |
| — | rebase 体系 | 见 §6 | 🚫 明确不做 |

---

## 一、上游结构（排查台账）

| 上游文件 | 行数 | 内容 | 处置 |
|---|---|---|---|
| `src/diff.rs` | 570 | `git diff` 调用、解析、`ChangeOp`/`apply_operations`（rebase 写回）、upstream 检测、`perform_rebase` | 只留**前两项**（`giff/diff.rs`，264 行含测试） |
| `src/ui/render.rs` | 984 | header / 文件列表 / 双栏 / unified / rebase 通知模态 / 帮助模态 / 键位行 / 行对齐 | 去掉 rebase 通知，其余搬（`giff/render.rs`，833 行） |
| `src/ui/rebase.rs` | 390 | 变更配对（删除/新增配对 + context）、rebase 视图渲染 | **不搬** |
| `src/ui/event_loop.rs` | 572 | crossterm 事件循环、按键分发、鼠标滚轮、`commit_rebase_changes` | 只搬**导航键语义**（重写成 `handlers.rs` 的 `giff_navigate`） |
| `src/ui/theme.rs` | 366 | 自带 dark/light 主题、`ThemeConfig`、`by_name` | 换成 `GiffStyle::from_theme(&prux Theme)` |
| `src/ui/syntax.rs` | 153 | `two_face` 语法集 + `HighlightLines` 逐行高亮 | 换成 `render::syntax_hl::session_for_filename` |
| `src/ui/types.rs` | 60 | `AppMode`/`ChangeState`/`Change`/`App`/`Pane`/`ViewMode` | 只留 `Pane` / `ViewMode`（`giff/types.rs`） |
| `src/ui/mod.rs` | 118 | 终端 setup/restore、panic hook、`App` 构造、主题轮换 | 不搬（宿主已接管终端） |
| `src/ui/tests.rs` | 561 | `TestBackend` 渲染断言 | 重写为 `giff/tests.rs`（263 行，13 个单测） |
| `src/config.rs` | 126 | `~/.config/giff/config.toml` 与自定义主题 | **不搬**（用 `/settings` 与主题文件的既有机制） |
| `src/main.rs` | 94 | clap 参数、rebase 前置检查、主题解析 | **不搬**（入口是 `/giff`） |

---

## 二、落点映射（逐文件）

| prux 落点 | 内容 | 与上游的对应 |
|---|---|---|
| `src/modes/interactive/giff/mod.rs` | `GiffView` 状态与导航方法、`GiffStyle`、`GiffRestore` | 上游 `ui/types.rs` 的 `App`（去掉 rebase 字段）+ `ui/theme.rs` 的取色 |
| `src/modes/interactive/giff/diff.rs` | `FileChanges` / `LineChange` / `parse` / `collect_uncommitted`（含 4 个单测） | 上游 `src/diff.rs` 的采集与解析部分 |
| `src/modes/interactive/giff/render.rs` | `render` + header / 列表 / 双栏 / 统一 / 帮助 / 键位行 / 高亮 | 上游 `ui/render.rs` + `ui/syntax.rs` |
| `src/modes/interactive/giff/types.rs` | `Pane` / `ViewMode` | 上游 `ui/types.rs` 的同名枚举 |
| `src/modes/interactive/giff/tests.rs` | 对齐 / unified / 截断 / 恢复 / 渲染 / 键位（13 个单测） | 上游 `ui/tests.rs` |

上游的 `AppMode`（`Diff`/`Rebase`）、`ChangeState`、`Change`、`rebase_changes`、
`current_change_idx`、`show_rebase_modal`、`rebase_notification`、`status_message`、
`theme_cycle` 一律不进 prux。

---

## 三、键位重写

上游键位围绕「Diff 模式 / Rebase 模式」两态设计；移植后只剩浏览态与输入态，
并按 prux 的约定重新分配（`Esc` 让给输入态、退出改用 `q`）：

| 键 | 上游 | prux | 理由 |
|---|---|---|---|
| `q` | 退出 | 退出视图（保存位置） | 与 prux 模态面（面板/选择器）一致 |
| `Esc` | 退出（Diff）/ 返回（Rebase） | 输入态：退回导航；导航态：不做事 | 退出交给 `q`，避免误按丢视图 |
| `i` | 无 | 显示并激活主页输入框 | 需求：看 diff 时能直接提问 |
| `Enter`（输入态，有内容） | 无 | 提交消息并退出视图 | 回复要落在主页聊天区 |
| `Enter`（输入态，空） | 无 | 不做事（不退出视图） | 避免误按空回车掉出视图 |
| `Ctrl+C` | 无（上游靠 `Esc`/`q`） | 与 `q` 同义退出 | prux 的通用退出键 |
| `t` | dark/light 轮换 | 删除 | 主题跟随 prux |
| `r` / `a` / `x` / `n` / `p` / `c` | rebase 体系 | 删除 | 见 §6 |
| `j`/`k`/`↑↓`、`PgUp`/`PgDn`、`Home`/`End` | 按焦点作用于列表或内容 | 同（翻页步长改用内容区高度而非终端高度 − 6） | 语义不变，参数本地化 |
| `Tab`、`h`/`l`、`←`/`→` | 焦点切换 | 同 | 不变 |
| `u` | 双栏 / unified | 同 | 不变 |
| `?` | 帮助模态 | 同（`?`/`q`/`Esc` 关闭） | 不变 |
| 鼠标滚轮 | 按 `column < width/5` 判断窗格 | 按上一帧记录的**文件列表区域**判断 | 命中判定用真实布局，不再用比例猜测 |

键位是**硬编码**的：不读 `keybindings.json`（上游也没有该机制，见 §7）。

---

## 四、核心改动清单

视图本体之外，prux 侧共 7 个接入点（全部为最小接线，无新抽象）：

| 文件 | 改动 |
|---|---|
| `src/modes/interactive.rs` | `pub mod giff;` |
| `src/modes/interactive/app.rs` | 字段 `pub giff: Option<GiffView>`、`pub giff_restore: GiffRestore` 与 `App::new` 初始化 |
| `src/modes/interactive/render.rs` | `render_fullscreen_giff`（**不渲染 banner**、保留底栏、输入态插主页输入框与光标）；在 `render_frame_widgets` 里分流；`mod syntax_hl` 提为 `pub(crate)` |
| `src/modes/interactive/handlers.rs` | `handle_event` 的键盘分流 + `handle_giff_key` / `giff_navigate` + `App::close_giff` |
| `src/modes/interactive/handlers/mouse.rs` | `handle_mouse_event` 的 giff 分支 + `App::giff_wheel` + `rect_contains` |
| `src/modes/interactive/handlers/commands.rs` | `BUILTIN_COMMANDS` 加 `giff`、“Review uncommitted changes”、分发到 `cmd_giff` |
| `src/modes/interactive/render/syntax_hl.rs` | 新增 `session_for_filename`（按文件扩展名取语法，供 diff 按文件高亮） |

**没有**新增扩展（无 `EXTENSION_FACTORIES` 条目、无 `PRIORITY_*`、无模式门控、无 `/extension`
面板条目）：这是宿主界面，不是可开关的功能策略。渲染分流放在全屏覆盖层（`OverlaySize::Fullscreen`）
判断之前，两者不会同时存在。

---

## 五、主题与语法高亮

### 5.1 主题：只读当前主题，不建主题

上游 `ui/theme.rs` 自带 dark/light 与 `config.toml` 自定义；prux 版把它的每个字段映射到
既有语义键（`GiffStyle::from_theme`，`mod.rs`）：

| giff 字段 | prux 语义键 | 说明 |
|---|---|---|
| `accent` / `fg_key` | `accent` | 徽标、焦点标题、键位行按键名 |
| `border_focused` / `border_dim` | `borderAccent` / `borderMuted` | 窗格边框 |
| `fg_normal` / `fg_bright` / `fg_dim` | `text` / `text` / `dim` | 正文、当前文件名（加粗在渲染点叠加）、次要文本 |
| `fg_added` / `bg_added` | `toolDiffAdded` / `toolSuccessBg` | 新增行的前景与整行底 |
| `fg_removed` / `bg_removed` | `toolDiffRemoved` / `toolErrorBg` | 删除行的前景与整行底 |
| `fg_added_marker` / `fg_removed_marker` | `toolDiffAdded` / `toolDiffRemoved` | `+` / `-` 标记 |
| `bg_header` / `bg_selection` / `bg_key_badge` | `selectedBg` | header 行、选中行、按键徽章 |
| `bg_modal` / `border_modal` | `customMessageBg` / `borderAccent` | 帮助模态 |
| `fg_separator` / `fg_line_num` / `fg_badge` | `borderMuted` / `dim` / `text` | 分隔线、行号、徽章文字 |
| `bg_default` / `bg_modal_dim` | `Color::Reset` | 终端默认背景（prux 主题没有「压暗背景」语义键，模态外不额外压暗） |
| `syntax_theme` | `syntax_hl::build_theme(&App.theme)` | 语法高亮主题 |

约定与实现细节：

- **不新增主题键、不改 `assets/themes/**`**：缺键一律走 fallback（`from_theme` 的第二参数）；
- 空串（终端默认色的表示法）映射为 `Color::Reset`；
- `GiffStyle` 每帧重建，只是「取色快照」，不是主题定义、不落盘、不可配置；
- syntect 主题按 `Theme::render_fingerprint() + syntax_highlight` 缓存（`syntect_fp`），
  避免每帧两次重建 `ScopeSelectors`。

### 5.2 语法高亮：换掉 two_face，并修掉上游的变体隐患

- 上游用 `two_face::syntax::extra_newlines`（130+ 语言）配 `HighlightLines` **逐行**喂入；
  prux 的 `syntax_hl` 用的是 `load_defaults_nonewlines`（~75 语言）+ 内置精简 Zig 语法，
  并在模块注释里写明：**逐行喂入必须用 nonewlines 变体**，否则以「行尾 `\n`」为退栈条件的上下文
  （如 C++ 的 `meta.preprocessor.include`）永远不弹栈、整块高亮塌陷。上游用的正是 `newlines` 变体，
  移植时按 prux 的正确做法落地。
- 新增 `session_for_filename`：`SyntaxSet::find_syntax_for_file` 按路径取语法（上游的同一入口），
  未识别语言返回 `None`、调用方回退纯文本。
- 行渲染仍按上游：`{行号:4}` + `- `/`+ ` 标记 + 正文；上下文行剥掉 diff 的前导空格再高亮，
  保证缩进敏感语言（Python / YAML）正确着色；整行铺 `bg_removed` / `bg_added`。

---

## 六、明确不做（决策记录）

| 不做的事 | 上游位置 | 理由 | 翻案条件 |
|---|---|---|---|
| rebase：逐块 accept/reject、`ChangeOp`、`apply_operations`、`apply_changes`、写回工作树 | `ui/rebase.rs`、`ui/event_loop.rs`、`diff.rs` | 视图定位是只读审阅；写工作树需要与 prux 未落盘的编辑、工具链路协同，风险与收益不匹配 | 出现明确需求时，作为**独立命令**（如 `/giff-rebase`）重开，不改本视图的只读语义 |
| `c` 提交（`git commit`） | `ui/event_loop.rs` | 同上；提交应由用户自己在 shell 或 prux 的工具链里做 | 同上 |
| `--auto-rebase`：`check_rebase_needed` / `get_upstream_branch` / `perform_rebase` | `diff.rs`、`main.rs` | 会在启动时改分支状态，属主动写操作 | 若要做，只保留**提示**不自动执行 |
| `~/.config/giff/config.toml` 与 giff 自带主题 | `config.rs`、`ui/theme.rs` | 与 prux 的主题/设置机制重复；用户已能在 `/settings` 与 `assets/themes` 里调 | 不翻案 |
| `t` 主题轮换（dark/light/自定义循环） | `ui/event_loop.rs` | 同上；视图跟随当前主题 | 不翻案 |
| clap 参数：`<from> <to>`、`-d "<args>"`、`--theme` | `main.rs` | 本次只做「未提交改动」入口；带参数的形态需要命令解析与提示文案对齐 | 需要时扩成 `/giff <ref>`，复用 `diff.rs` 的解析 |
| 扩展化（linkme 工厂 / `Dev`·`Creator` 模式 / `/extension` 条目 / busy_safe 声明） | — | 这是宿主界面而非可开关策略；做成扩展会让「界面归 TUI 层」的分层变模糊 | 不翻案 |
| `assets/demo.gif` 等上游演示素材 | `assets/` | 本仓不上传演示素材；changelog 与文档足够 | 不翻案 |

---

## 七、缺口与未做项

1. **位置恢复不跨进程**：`GiffRestore` 只活在 `App` 里（`q` 退出时写入），重启 prux 后
   `/giff` 从头开始。要跨重启需落盘到 `agent_dir()` 下的状态文件（并考虑工作树已变的失效语义）。
2. **键位不接 `keybindings.json`**：`j`/`k`/`u`/`i`/`q` 等硬编码，用户不能改绑（上游也无此机制）。
   若要接，需把 `giff_navigate` 换成 `keybindings::matches` 判定，并为每个动作加 id。
3. **鼠标只支持滚轮**：不支持下拖选文本、不支持点击切换文件/焦点；文件列表命中用上一帧区域矩形。
4. **帮助模态按高度截断**：窄终端（< 26 行）下条目会被 `lines.truncate(inner.height)` 砍掉，
   不做滚动。
5. **`/giff` 非 busy_safe**：agent 正在流式输出时该命令按 prux 既有规则入 steer 队列，
   不会抢占主循环。
6. **无 rebase 提示**：上游会在分支落后/分叉时弹提示，移植版不检测 upstream（见 §6）。
7. **`page_rows` 依赖上一帧**：内容区高度由渲染期写入，首帧之前翻页步长为 1；
   实际不可达（首帧必先渲染）。

---

## 八、验证记录

- **单测 18 个**：`giff/diff.rs` 4 个（文件切分与上下文、hunk 行号、元数据/`\ No newline` 丢弃、
  空输入）+ `giff/tests.rs` 13 个（对齐配对与补齐、unified 顺序、路径截断含全角、
  整帧 header/列表/键位行断言、`u` 切换、输入态键位行、状态恢复与回落、导航夹取、滚动夹取、
  帮助模态）+ `handlers.rs` 1 个（`i` 进输入态、字符入框、`Esc` 只退输入态、`q` 关闭并置 `full_redraw`）。
- **全量**：`cargo test` 3 568 个 lib 测试 + 全部集成测试通过；`cargo fmt --check` 干净；
  `cargo clippy --all-targets` 对新增文件零警告（既有警告不变）。
- **未做的验证**：真机 TUI 交互（本仓约定不用 tmux 调试 TUI；渲染走 `TestBackend` 断言，
  交互走 `handle_event` 级别的测试）。首次人工验收需覆盖：`/giff` 进入、`i` 输入、`Esc` 退回、
  `q` 退出、再次 `/giff` 是否回到原文件与滚动位置。
- **changelog**：`assets/changelog/v.1.1.0.md` 的 Added 段已加中英条目（隐藏 banner、
  保留底栏、只读、位置恢复）。
