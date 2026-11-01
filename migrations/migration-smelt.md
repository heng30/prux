# Prux 迁移评估：smelt → prux 扩展

> **来源**：`smelt`（`https://github.com/leonardcser/smelt`，MIT，Lua 插件式终端 agent；
> 本仓的本地只读检出是 `tmp/smelt`，不在 git 索引内）。
> 规模参考：workspace 下 15 个 crate（`core` / `tui` / `edit` / `term` / `buffer` / `provider` /
> `engine` / `store` 等），运行时插件与工具是 `runtime/lua/**` 下的 Lua 脚本。
>
> **目标**：判断 smelt 的功能面里，哪些能收编成 `src/extensions/` 的 Rust 扩展。
>
> **性质**：**评估档**。§3 的 5 项缺口已逐项定论：第 1 项（后台进程监督）已落地，
> 第 2–5 项**明确不做**；落地状态见 §4，不做的理由并入 §5。
>
> **与既有迁移文档的关系**：`migration-giff.md` / `migration-subagents.md` / `migration-tasks.md`
> 是「单点上岸」，本档是**全量功能面扫描后的差集清单**，结论多为「不做」，因此不写移植步骤，
> 只写判据。

---

## 〇、摘要

### 结论

**机制不可移植，只能移植功能**：smelt 的扩展性来自内嵌 Lua（`mlua` + vendored Lua 5.4），
键位、命令、工具、UI 全是运行期 Lua 脚本；prux 的扩展是**编译期静态注册**
（`src/extensions.rs` 的 `#[linkme::distributed_slice]` 工厂切片 + `priority` 降序）。
所以「把 smelt 的插件搬过来」不成立，只能把某个功能用 Rust 重写成一个扩展。

**功能面高度重合**：prux 已有的 38 个扩展已经覆盖了 smelt 大部分显性能力
（见 §2）。真正的缺口只有 5 项，按值得做程度排序：

1. 后台进程监督（`/ps` + bash 后台任务 + 输出读取/停止 + 前台脱离）；
2. Notebook（`.ipynb`）读取渲染与 cell 编辑；
3. `/inspect` 本地 Web UI（会话浏览 + provider 请求/响应审计）；
4. 输入预测（小模型预测用户下一条消息，占位符呈现，Tab 接受）；
5. 托管 worktree（`/worktree` + `enter_worktree` / `switch_cwd` 工具）。

⚠️ **许可**：smelt 是 MIT。真搬代码时要保留其版权与许可声明（本仓根 `LICENSE` 之外单独注明来源）。

---

## 一、为什么不能搬插件层

| 维度 | smelt | prux |
|---|---|---|
| 扩展形态 | `~/.config/smelt/**/*.lua` 运行期加载，`smelt.plugins.<name>` 可 `disable` | `src/extensions/<name>.rs` 编译期静态注册，`/extension` 面板开关 |
| 注册方式 | `require(...)` + 运行期 API 表 | `linkme` 分布式切片 + `priority` 降序 |
| 键位/命令 | Lua 里 `smelt.keymap.set` / `smelt.cmd` 注册，可 `which_key` 提示前缀 | 固定键位表 + `BUILTIN_COMMANDS` + 扩展 `ExtensionCommand` |
| 工具 | Lua 函数 + JSON schema | Rust `ExtensionTool` |
| 渲染 | 自研 grid/layout（不用 ratatui） | `ratatui` |

推论：**只能把语义与交互搬成 Rust 扩展**；Lua API 表（`runtime/lua/smelt/_meta/**`）是参考文档，
不是可移植资产。

---

## 二、已有等价物（明确不移植）

| smelt | prux 对应 |
|---|---|
| `smelt.plugins.which_key`、`/help`（F1） | `/hotkeys` + `assets/docs/keybindings.md` |
| `/diff`（`smelt.plugins.diff`） | `/giff`（`src/modes/tui/giff.rs` / `src/modes/gui/giff.rs`） |
| `smelt.plugins.compact`、`/compact` | 核心压缩 + `/compact` |
| `smelt.plugins.turn_notifications`、`/notify` | `notify` 扩展 |
| `smelt.plugins.terminal_title` | `src/modes/tui.rs::update_terminal_title` |
| `smelt.plugins.upgrade`、`/upgrade` | `update_check` 扩展 |
| `/version`、`/changelog` | `--version`、`/changelog` |
| `/rewind` | `rewind` 扩展 |
| `smelt.plugins.plan_mode`、`present_plan` | `plan_mode` 扩展 |
| `smelt.plugins.goal`、goal 工具族 | `goal` 扩展 |
| `smelt.plugins.lsp`（outline/defs/refs/diagnostics/rename） | `lsp` 扩展 |
| `ask_user_question` 工具 | `ask_user_question` 扩展 |
| `/history`（Ctrl+R） | `/history` |
| `/messages` | 错误/告警面板（`handlers/messages.rs`） |
| `/skills`、`load_skill` | `skills` 扩展 |
| `/permissions` | `permissions` 扩展 |
| `/session`、`/trust`、`/reload`、`/export`、`/copy`、`/model`、`/theme`、`/tree`、`/usage` | 同名内置命令或 `usage` / `session_search` 扩展 |
| `perf_panel`（F12）、`debug_panel`（F3） | `debug` 扩展 + `handlers/metrics.rs` |
| `web_fetch` / `web_search` | `web_access` 扩展 |
| `mcp` | `mcp` 扩展 |
| `runtime/skills/handoff` | `assets/skills/handoff` |
| `/brief`、`/reflect`、`/simplify`（技能） | `ponytail` / `vet` / `assets/skills/code-review`，语义重叠 |
| `esc_chord`（Esc-Esc） | `rewind` 扩展已有回退语义，只差双击键位 |

---

## 三、缺口清单（按值得做程度排序）

### 1. 后台进程监督 ⭐ 最值得做 —— ✅ 已落地

**smelt**：`bash` 工具带 `background` / `background_on_timeout`，配套 `read_process_output`
（读快照、不 drain）、`stop_process`、`/ps`（PID + 运行时长 + 有界输出）、Ctrl-G
（`process_control` 插件：停止跟随前台命令但让它继续跑）。每个 job 在 Linux 上走独立
systemd scope，Unix 上有进程组兜底、Windows 有 Job Object；并用 cgroup OOM 与普通信号区分；
64 并发上限、完成后输出进有界最近任务缓存。

**prux 现状**：`src/core/tools/bash.rs` 只有前台执行 + 超时**杀进程**（`kill_process_tree`），
没有后台 job、没有 job id、没有 `/ps`。`hang_detect` 是假死检测，不是进程管理。

**成本**：上游 `crates/core/src/process.rs` 3408 行 + `processes.rs` 89 行 + `tools/bash.lua` 214 行。
可砍的部分：systemd scope、cgroup OOM 区分、Job Object（先只用进程组）、recent-job 缓存淘汰策略。

**分期**：先只做「后台 job + job id + 读输出 + 停止 + `/ps` 列表」，
`background_on_timeout` 与「脱离前台」后续再加。

**落点**：新扩展 `src/extensions/processes/`（`/ps` dock 面板 + 两个工具 + 脱离键位），
`bash` 的 `background` 参数则要动 `src/core/tools/bash.rs` —— 这是核心工具，**不属于扩展**，
需要先在扩展里注册一个独立的后台执行工具，或明确接受改核心。

### 2. Notebook（`.ipynb`）—— ❌ 明确不做

**smelt**：`read_file` 把 `.ipynb` 渲染成带编号的 cell（类型 + 源码 + outputs）；
`edit_notebook`（replace/insert/delete，按 `cell_number` 0 起或 `cell_id`，insert 插在该 cell 之后）；
`write_file` / `edit_file` 明确拒绝对 `.ipynb` 操作。

**prux 现状**：完全没有 notebook 概念；`markit` 扩展只管 PDF/Office → Markdown。

**成本**：`crates/core/src/notebook.rs` 1709 行 + `tools/notebook_edit.lua` 188 行。
其中相当一部分是 cell schema 解析与 diff 呈现，可复用 prux 的 `utils/diff`。

**落点**：新扩展，注册 `edit_notebook` 工具 + 一个「读 notebook」工具，自己管 JSON 解析，
不碰 `read`/`write` 核心工具（否则又变成改核心）。

### 3. `/inspect` 本地 Web UI —— ❌ 明确不做

**smelt**：opt-in 插件起本地 HTTP 服务，浏览器里翻会话列表、历史，以及 provider 请求/响应审计数据。

**prux 现状**：有 `proxy` 扩展的 dock 统计、`export_html` 的静态导出、`session_search`
的会话内检索，**没有 web 端**；也没有把 provider 请求原文落盘并可视化的能力。

**成本**：`crates/tui/src/inspect_server.rs` 1343 行 + `crates/tui/src/inspect/` 前端约 1150 行
（vanilla JS + CSS）；prux 侧还要自己记请求/响应审计（`proxy` 的 stats 不够）。

**评价**：独立性最强、最「扩展相」（自成一体的 HTTP + 静态资源，与 TUI 解耦），
但也是清单里量最大的一项。**建议等有明确需求再做**，否则是高投入的猜测。

### 4. 输入预测 —— ❌ 明确不做

**smelt**：`predict` 插件（110 行 Lua）后台调小模型，把预测的下一条用户消息渲染成输入框
placeholder；Tab 接受、Esc/Ctrl-C 丢弃、打字只是隐藏（撤销回空 buffer 会重新出现）。
system prompt 固定，最近对话作为单条 user 消息引用（避免模型接着 assistant 往下写）。

**prux 现状**：无。`handlers/suggest.rs` 只是斜杠命令补全，不是内容预测。

**成本**：语义便宜（提示词 + 一次后台调用），但要动 TUI 输入框的 placeholder/Tab/撤销交互，
属于 `modes/tui/line_input.rs` 的地盘。**做成扩展需要先确认 line_input 有给扩展的接缝**，
没有的话得改核心。

### 5. 托管 worktree —— ❌ 明确不做

**smelt**：`/worktree [name] [--base ref]` 选/建/进；`enter_worktree` 工具建好并等于 `switch_cwd`；
`switch_cwd` 切真实进程 cwd（区别于 bash 里的 `cd`）。

**prux 现状**：只有 `subagent` 的 `isolation: worktree`（内部建副本、结束即删/提交到分支）；
TUI 有 `AgentCommand::SwitchCwd`（UI 驱动），但**没有 agent 可调的 `switch_cwd` 工具**，
也没有用户侧 `/worktree` 命令。

**成本**：中。git worktree 建立/命名归一化可复用 `subagent` 的现成逻辑；难的是语义
（切 cwd 影响项目指令、技能、权限判定、会话元数据）。

---

## 四、落地状态

### 1. 后台进程监督 —— 已落地（`src/extensions/processes.rs`）

只做了 §3 的第 1 期，且**不改核心 `bash` 工具**（§6 前置检查里那条边界按「新增独立工具」处理）：

| 能力 | 落点 |
|---|---|
| 后台启动 | 工具 `background_bash`（`command` + 可选 `cwd`），立即返回 `job-N` / pid / 日志路径 |
| 读输出 | 工具 `read_process_output`（快照式、不 drain；省略 `id` 列全部 job），日志尾部行数 `lines`（默认 100、上限 2000、单次最多 256 KiB） |
| 停止 | 工具 `stop_process`，Unix `killpg` / Windows `taskkill /T`，连同子进程一起杀 |
| 用户视图 | `/ps`（打印清单 + 打开 dock 面板）、`/ps stop <id>`、`/ps clear` |

砍掉的部分：`background_on_timeout`、Ctrl-G「脱离前台」键位、systemd scope、cgroup OOM 区分、
Windows Job Object（只用 `taskkill /T`）。

与 smelt 的差异（有意）：

- **不改内置 `bash`**：`background` 参数不进 `src/core/tools/bash.rs`，后台执行是独立工具，
  语义上「一步到位进后台」而不是「前台跑着跑着转后台」；
- **job 不随 prux 退出被回收**（与 `spawn_detached` 同语义）；禁用扩展才统一停掉运行中的 job，
  避免工具面消失后用户停不了它们；
- 输出走 `agent_dir()` 外的临时日志文件（0600）而不是 systemd journal。

### 2–5 —— 明确不做

Notebook、`/inspect` Web UI、输入预测、托管 worktree 四项本轮明确不做，理由并入 §5。

---

## 五、明确不做

| 项 | 原因 |
|---|---|
| Lua 插件/配置体系（`mlua`、`_meta` API 表、`init.lua`） | 与 prux 编译期扩展架构不兼容 |
| `snake`（F11 彩蛋） | 无价值 |
| `scroll_pills`、`block_nav`、`agent_view` | prux 有自研 TUI 的滚动/导航/子代理视图，不共用模型 |
| Claude Code backend（把 `claude` CLI 当 agent 后端，smelt 只提供 TUI + 权限 + MCP 工具） | 与 provider 抽象冲突，且依赖外部二进制与外部鉴权 |
| `custom_commands`（Lua 注册斜杠命令） | prux 的 `prompt_templates` 已覆盖「用户自定义 `/命令`」 |
| `/fast`（加速推理开关）、`/color`、`/nohl`、`/docs`、`/btw` | 各自绑定 smelt 的 provider/UI 细节；`btw` 语义已由 `recap_btw` 扩展覆盖 |
| `brief` / `reflect` / `simplify` / `customize` 技能 | 与 `assets/skills/**` + `ponytail`/`vet` 重叠，`customize` 更是讲 smelt 自己的 Lua 配置 |
| Notebook（`.ipynb` 读取 + `edit_notebook`） | 需求窄（只有 Jupyter 用户），且与内置 `read` / `edit` 的语义要长期对齐；宁可让模型自己读写 JSON |
| `/inspect` 本地 Web UI | 投入最大（HTTP 服务 + 前端 + provider 请求/响应审计落盘），却没确定的需求，属高投入猜测 |
| 输入预测 | 要动 `modes/tui/line_input.rs` 的 placeholder / Tab / 撤销交互，扩展没有接缝；收益是「省几次按键」 |
| 托管 worktree（`/worktree` + `switch_cwd` 工具） | 切真实 cwd 会牵动项目指令、技能、权限判定与会话元数据，语义成本远高于建 worktree 本身 |

---

## 六、若要动手的前置检查

1. **是否必须改核心**：清单 1（bash 后台）与 4（输入预测）都碰到核心工具/输入框。
   已定：接受「扩展只能新增工具，不能改内置 bash」，第 1 项因此做成独立工具
   `background_bash`（语义与 smelt 的 `bash background` 不同）；第 4 项因需要输入框接缝
   且无接缝而明确不做。
2. **默认禁用**：按仓库约定，新扩展注册后默认禁用，需用户在 `/extension` 开启。
3. **许可**：MIT，搬代码保留来源声明。
4. **测试**：进程管理与 Notebook 都要有确定性测试；
   TUI 相关的用 `ratatui::backend::TestBackend`，不要用 tmux 调试。
