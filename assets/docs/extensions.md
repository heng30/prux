# 扩展（Extensions）

prux 的扩展采用**编译期静态注册**：扩展作者用 Rust 实现 `Extension` trait（提供工具、事件钩子、斜杠命令、快捷键、CLI flag），再在应用入口处调用 `register_extension` 注册到全局注册表。扩展代码随 prux 二进制一起编译，无法在不重新编译的情况下增删——没有运行时加载、自动发现目录或热更新。

prux 的扩展模型只有一套：Rust trait + 全局静态注册表 + 启用/禁用状态 + 扩展模式。

## 核心概念

### 扩展接口

扩展分三类，都通过静态注册进入全局注册表：

- **`Extension`（工具/钩子扩展）**：提供模型可见的工具、事件钩子、斜杠命令、快捷键、CLI flag。如 `plan-mode`、`extra-themes`、`subagent`。
- **`FooterExtension`（底栏扩展）**：接管 TUI 底栏渲染，并接收 agent 内核事件。如 `footer(normal)` / `footer(minimal)` / `footer(rich)`。**互斥**：同一时刻至多一个 footer 生效。
- **`BannerExtension`（横幅扩展）**：干净启动时在 TUI 窗口顶部渲染一次性的 ASCII 横幅（`assets/banner/normal.txt`）。如 `banner`。**互斥**：同一时刻至多一个 banner 生效。

三类扩展共用同一套启用状态与扩展模式，都出现在 `/extension` 面板中。

### 注册

在应用组装处调用注册函数（prux 主程序在 `main` 的 `extensions::register_all()` 里遍历 linkme 分布式切片、按 `priority` 降序统一注册）：

```rust
// 工具/钩子扩展
core::extensions::register_extension(extensions::plan_mode::PlanMode::new());

// 底栏扩展
core::extensions::register_footer_extension(extensions::footer::NormalFooter::new());

// 横幅扩展
core::extensions::register_banner_extension(extensions::banner::Banner::new());
```

注册时按 settings 恢复持久化的启用状态（默认启用；声明 `default_enabled() == false` 的扩展默认关闭，需 `enabledExtensions` 或 /extension 面板显式开启），并回调 `on_enabled_changed` 与 `on_registered`（接线命令/快捷键执行入口）。

### 分发顺序

内置工具（`read` / `bash` / `edit` / `write` / `grep` / `find` / `ls`）保持原实现不动；扩展作为独立第二层接入。工具调用**先查内置、再查扩展注册表**。`registered()` / `registered_footers()` / `registered_banners()` 只返回「已启用且当前模式可用」的扩展，因此钩子分发、工具分发、底栏/横幅渲染、命令/快捷键候选都自动按状态过滤。

## 内置扩展

| 扩展名 | 类型 | 说明 |
|--------|------|------|
| `footer(normal)` | footer | 默认底栏：行 1 工作目录（含 git 分支），行 2 token/缓存/费用/上下文统计 + 模型名 |
| `footer(minimal)` | footer | 只显示模型行（normal 的第二行） |
| `footer(rich)` | footer | 富底栏：分支/模型/上下文 + token 速率/费用/目标标记 🎯/计划模式标记 📋/交互数/耗时/工具调用计数/LLM 调用计数 📨 |
| `banner` | banner | 启动横幅：干净启动时窗口顶部左侧显示 `assets/banner/normal.txt` 的 ASCII art（`accent`），宽度足够时 art 右侧 4 列间隔处画竖线 + 6 行信息区（版本加粗显示、描述、常用工具、`/extension` 提示、快捷键提示）；顶部/底部各 2 行、左侧 4 列 padding；声明 `Minimal`（即所有扩展模式都可用）；首次提交输入后隐藏 |
| `extra-themes` | 扩展 | 把编译期嵌入的额外主题同步进 `agent_dir()/themes`（见 [themes.md](themes.md)）；启用写入、禁用删除，内容幂等、不覆盖外部定制 |
| `skills` | 扩展 | 统一的技能管理面板（`/skills`）：列出全部技能（含尚未落盘的扩展技能），空格切草稿、`Ctrl+S` 写 `settings.json` 的 `skills` 过滤数组并重建 skill 工具、回车看详情（SKILL.md 头信息 + 来源 + 路径）。随二进制分发的**扩展技能**（`assets/skills/**`，编译期内嵌）默认不落盘，开启时「检查并创建」到 `agent_dir()/skills/<name>/**`（已存在不覆盖）。**默认启用**。详见 [skills.md](skills.md) |
| `downloader` | 扩展 | 外部工具（`fd` / `rg`）的安装与状态展示：`/download fd\|rg` 缺失时直接下载、已存在时弹选择面板（重装 / 取消）、下载中只提示、未知工具与多余参数报错；裸 `/download` 切换本扩展的 dock 段显隐（对齐 plan-mode `/todos`、proxy `/proxy`，不控制整个面板），段内每行是「状态点 + 工具名 + 版本 + 位置」（仅 PATH 命中的系统副本会标 `(system)`）。**默认启用**（工具可用性属于开箱能力）；禁用只影响该命令与 dock 段，启动时的缺失检测与提示（不自动下载）不受影响 |
| `update-check` | 扩展 | 启动时异步检查 GitHub 最新 release（`heng30/prux`），有新版本就在**输入框下方**弹出选择面板（`Open download page` / `Ignore` / `Don't ask again`；↑↓ 选择、Enter 确认、Esc/Ctrl+C 关闭）。`Ignore` 只关掉本次提示，`Don't ask again` 把版本写进 `settings.json` 的 `skipUpdateVersion`（只抑制该版本，之后更新的版本仍会提示）。`--offline`/`PRUX_OFFLINE` 跳过检查，网络失败静默。**默认启用**。另提供 `/update check`（异步检查并在聊天区输出当前/新版本与下载地址）与 `/update open`（直接打开最新 release 下载页，不比较版本） |
| `doc-helper` | 扩展 | 精简系统提示词：启用后从默认系统提示词移除整段 prux 文档描述（README/docs/examples 指针 + 主题映射），改用 `/help <question>` 按需提问——把**同一段**文档描述当作一条普通 user 消息投出（不动系统提示词，保护缓存命中率）；无参 `/help` 只列出文档主题。**默认不启用**，需在 `/extension` 面板开启 |
| `plan-mode` | 扩展 | 只读探索模式：禁用 `edit`/`write`、`bash` 白名单、`Plan:` 编号步骤 + `[DONE:n]` 进度跟踪（`/plan [text]`、`/todos`、`Alt+P`、`--plan`）。**默认不启用**，需在 `/extension` 面板开启 |
| `goal` | 扩展 | 持久化自主目标（移植自 pi-goal v0.1.7）：`/goal [--tokens 50k] <objective>` 设置长期目标，agent 持续工作直到完成/暂停/清除/预算耗尽。裸 `/goal` 开关停靠面板，`/goal pause\|resume\|clear` 换挡；提供 `create_goal`/`get_goal`/`update_goal` 工具。**默认不启用**，需在 `/extension` 面板开启 |
| `subagent` | 扩展 | 子代理：`Agent` / `get_subagent_result` / `steer_subagent` 工具，外加确定性编排器 `SubagentWorkflow` 与 `/agents` 命令。**默认不启用**，需在 `/extension` 面板开启。详见 [extensions/subagent.md](extensions/subagent.md) |
| `loop-detect` | 扩展 | 纯检测：实时检测模型自身的循环行为（流式 thinking/output 字符级与语义循环、跨轮推理停滞、工具调用序列循环），命中即 `AbortRun` 终止整个 run，并在聊天区发一条系统消息说明终止理由。不向会话写入模型可见内容、不改写/检索历史。配置在 `agent_dir()/extensions/loop-detect.json`，`/loop-detect` 打开扩展设置面板（无参数；预设档位、Space/Enter 循环切换、立即落盘）、`/loop-detect reset` 清状态。**默认不启用**，需在 `/extension` 面板开启 |
| `hang-detect` | 扩展 | 假死看门狗：run 进行中超过 `timeoutSecs`（等待模型）或 `toolTimeoutSecs`（工具执行中）没有任何 agent 事件，即认定假死，经 `RestartRun` 中止当前回合并**立即重开一轮**——新一轮的 user 批次 = 恢复引导消息 + 运行中 steer 注入箱（follow-up 保持「settle 后才发送」的语义，留在队列里不动）；连续恢复超过 `maxRecoveries` 次仍假死时退化为只 `AbortRun`（不重启）。计时心跳由独立后台线程驱动（注册时启动一次）。配置在 `agent_dir()/extensions/hang-detect.json`，`/hang-detect` 打开扩展设置面板，`/hang-detect status`、`/hang-detect timeout\|tool-timeout\|recoveries <n>`、`/hang-detect reset`。**默认不启用**（`Dev`/`Creator` 模式可用） |
| `notify` | 扩展 | agent 整轮结算（busy → idle，`agent_settled`）时执行一条外部通知命令。**只在真正空闲时通知**：结算时若仍有排队用户输入（steer / follow-up），UI 会立刻起下一轮，此时跳过通知，等队列排空、最后一次结算才发（耗时因此按整段忙碌期统计）。配置在 `agent_dir()/extensions/notify.json` 的 `settledCommand`（如 `notify-send prux "${message}"`），`${message}` 替换为程序生成的**固定文案** `会话名 · 状态 · 耗时`（如 `my-session · completed · 1m12s`；会话名未命名时回落 `prux`，状态取本轮最后一条 assistant 的 `stop_reason` → `completed`/`error`/`aborted`，耗时为整段忙碌期 `agent_start` → 空闲 `agent_settled`）。命令不经 shell、按空白切词后直接 `spawn`，`${message}` 保持单个参数（文案含空格也不会被拆开）；模板不含占位符时文案作为末位参数追加。`/notify show` 打印当前 `settledCommand`，`/notify settled <command>` 写入配置（如 `/notify settled notify-send prux ${message}`），`/notify reset [all|settled]` 清空命令（`all` 清全部、`settled` 只清 settledCommand；缺参报错）。`Esc`/`Ctrl+C` 硬中断会丢弃回合 future，不经过 `agent_settled`，因此**不通知**。**默认不启用**（`Dev`/`Creator` 模式可用） |
| `proxy` | 扩展 | **入站** OpenAI 兼容网关：`/proxy listen <host:port>` 设定监听、`/proxy provider <provider>` 设定目标 provider、`/proxy api-key <sk-xx>` 设定入站 token（裸命令清空），请求体 `model` 字段逐请求选模型，流式转发到该 provider（走 prux 的鉴权/OAuth/四协议适配）；裸 `/proxy` 开关 dock 面板（地址/连接数/请求数/速率/最近请求/api-key 遮蔽值）。配置在 `agent_dir()/extensions/proxy.json`。**默认不启用**，需在 `/extension` 面板开启。注意与**出站** HTTP 代理（[http-proxy.md](http-proxy.md)）方向相反 |
| `rewind` | 扩展 | 回退最后一次用户输入：把活动分支叶子移到最后一条 user 消息之前，该消息与由它产生的全部后续输出一起离开模型上下文（只动分支指针，历史仍保留在会话树，`/tree` 可找回）。`/rewind` 每次都弹确认面板（Rewind & edit / Rewind & discard / Cancel），忙碌时拒绝执行。**默认不启用**（`Dev`/`Creator` 模式可用） |
| `codemode` | 扩展 | JavaScript 工具编排：模型写一段 JS，在 QuickJS 沙箱里用 `tools.<name>()` 并行/串联调其它工具（内置、扩展、MCP 都能调），只把脚本输出交回模型；嵌套调用记进 `details.nestedCalls` 并计入会话成本。`codemode.mode`（`on`/`only`）与 `codemode.inlineBudget` 控制可调工具怎么声明给模型（配置在 `agent_dir()/extensions/codemode.json`）；`/codemode` 打开扩展设置面板（Space/Enter 循环档位、立即落盘），`/codemode show` 打印当前配置。**默认不启用**（`Dev`/`Creator` 模式可用），详见 [extensions/codemode.md](extensions/codemode.md) |
| `web-access` | 扩展 | 联网能力（移植自 pi-web-access v0.29.0，Phase 1）：`web_search`（8 个 REST provider 聚合搜索）/`fetch_content`（`readable` HTML→markdown、`raw` 原文，带 SSRF 防护）/`get_search_content`（按 `responseId` 取回内容切片、`findText` 检索）。配置在 `agent_dir()/extensions/web-search.json`（键名与上游一致）。**默认不启用**，需在 `/extension` 面板开启 |
| `mcp` | 扩展 | Model Context Protocol 客户端（移植自 kiss `kiss-mcp`）：用单个 `mcp` 工具代理已配置服务器的工具/资源/prompt（`status`/`list`/`search`/`describe`/`call`/`resources`/`read_resource`/`prompts`/`get_prompt`），懒连接 + 空闲断开 + 元数据缓存 + OAuth 2.1 登录；`/mcp` 做状态/诊断/OAuth，`prux mcp` 子命令做配置增删改查。**默认不启用**，需在 `/extension` 面板开启。详见 [extensions/mcp.md](extensions/mcp.md) |
| `jet` | 扩展 | 虚拟模型路由器（移植自 pi `examples/extensions/jev-router.ts`）：注册虚拟模型 `jet/auto`，**用强模型规划、用便宜模型实现**——本轮第一次成功的 `edit`/`write` 之后同轮切到实现模型并留在那里，因此每个会话最多切换一次模型、最多吃一次 prompt-cache miss。配置在 `agent_dir()/extensions/jet.json`（**没有默认值**：路由目标不完整或解析不到就不注册进 `/model`），`/jet set` 配分类器、`/jet router` 增量配路由目标（三档可各用不同 provider）、`/jet reset` 清空。**默认不启用**（`Dev`/`Creator` 模式可用）。详见下文与 [virtual-models.md](virtual-models.md) |
| `tasks` | 扩展 | 任务跟踪与协调（移植自 `@tintinweb/pi-tasks` v0.9.0）：7 个模型可见工具（`TaskCreate` / `TaskList` / `TaskGet` / `TaskUpdate` / `TaskOutput` / `TaskStop` / `TaskExecute`）、常驻任务 widget、`/tasks` 命令与系统提醒注入，`TaskExecute` 经 RPC 联动 `subagent` 扩展。**默认不启用**（`Dev`/`Creator` 模式可用） |
| `tool-search` | 扩展 | 延迟工具检索：扩展可把工具声明为 `ToolExposure::Deferred`（仍可执行但不进模型可见列表），本扩展提供 `tool_search` 工具按 BM25 检索延迟工具元数据，命中项在**下一次模型调用**变为可见。**默认不启用**（`Dev`/`Creator` 模式可用；无延迟工具时它只会占一个工具位） |
| `debug-provider` | 扩展 | `/debug-provider`：供应商原始流事件查看器，订阅 `ProviderStreamEvent`，每轮 assistant 消息结束时渲染一张卡片（provider/model/api + 捕获条数 + 事件摘要）并把整份载荷落成会话条目。**默认不启用**（仅 `Dev` 模式可用） |

> `subagent` 是功能最多的内置扩展，单独一篇文档：[extensions/subagent.md](extensions/subagent.md)——心智模型（三条派发路径）、运行环境与隔离、agent 类型与 frontmatter 键、工作流、设置键及其相互关系、界面、定时任务、事件与 RPC。

### loop-detect

`loop-detect` 是编译期内置的**纯检测**扩展（`Dev`/`Creator` 模式可用），不提供工具、不改变模型可见工具集，只通过钩子观察并干预：检测到循环即终止整个 run，除此之外不向会话写入任何内容。

- **流式检测**（`on_agent_event` 的 `message_update`）：扩展自行累积 `thinking_delta`/`text_delta`，每隔窗口一半的字符（10..=50）做一次字符级（Z 数组找相邻重复块）与语义级（空行段落指纹计数）检测；命中即经 [`ExtensionUiRequest::AbortRun`] 请求 TUI 丢弃当前回合（回合 future 被丢弃，半截消息不落库）。
- **跨轮检测**（`on_agent_event` 的 `message_end`）：扩展自己维护有界滑窗——最近 `stagnationWindow` 轮思考两两相似度；命中终止 run。滑窗是扩展自己观测到的实时快照，**不读取框架的消息数组**。
- **工具检测**（`before_tool_call_ext`）：只有"调用序列完全重复"一类；命中 `block` 该调用（唯一能阻止执行的机制）并同时请求 `AbortRun`。
- **不改写上下文**（`transform_context`）：本扩展只用它做每轮一次的 **run 上电探针**（`agent_start` 落在被门控的高频事件面上，需在低频面上上电），既不读取也不修改历史消息。
- **终止理由**：每次命中发一条 [`ExtensionUiRequest::Notify`] 聊天区系统消息（不落库、不进模型上下文），文本由代码按检测器生成。
- **状态重置**：`agent_end` / `agent_settled` / 会话开始或切换 / 被禁用时，检测状态整段归零。

配置键（camelCase）与开关见 `agent_dir()/extensions/loop-detect.json`（如 `thinkingWindow`、`stagnationWindow`、`toolLoop`；设为 `0` 即关闭）；每次检测还会向扩展事件总线广播 `loop-detect:detection` 载荷，并可选经 `hookCmd`（外部命令，JSON 载荷作为最后一个参数）与 `hookLog`（JSONL 追加）观察。

### hang-detect

`hang-detect` 是编译期内置的**假死看门狗**扩展（`Dev`/`Creator` 模式可用），**默认不启用**（需 `enabledExtensions` 或 `/extension` 面板开启）。provider 网络卡住、流式响应中断、工具挂死等情况下 run 会长时间不再产生任何 agent 事件，TUI 一直显示 “Working...”，用户只能手动 Esc；本扩展给“静默”设上限并自动恢复。

- **活动信号**：任何 agent 事件（`agent_start` / `turn_start` / `message_*` / `tool_execution_*` / `turn_end` …）都算活动并刷新计时；`auto_retry_start` 的退避等待按 `delayMs` 顺延，不误判为假死。
- **上下电**：`agent_start` 上电，`agent_end` / `agent_settled` 断电；会话开始 / 切换、扩展被禁用时整段复位。`maxRecoveries` 计数在**正常结算**（`agent_settled`）时清零，因此只有“连续假死且始终没能成功结算”才会耗尽上限。
- **计时心跳**：看门狗跑在一条**独立的后台线程**里（注册时启动一次，进程内单例），每 0.5s 唤醒一次检查静默是否超时；不依赖 TUI 事件循环，也不与其它扩展的渲染动画（`wants_redraw` 的 `any()` 会短路）争抢调用时机。
- **恢复**：命中即经 [`ExtensionUiRequest::RestartRun`] 请求 TUI 中止当前回合并立即重开一轮：新一轮的 user 批次 = 恢复引导消息 + 运行中 steer 注入箱（steer 本就是“运行中注入”语义）。**本地 follow-up 队列不动**——follow-up 的语义是“本轮 settle 后才发送”，保持中断前的行为，由新一轮结算后的 `drain_next_batch` 自然取走。连续恢复超过 `maxRecoveries` 次仍假死时退化为普通 [`ExtensionUiRequest::AbortRun`]（只中止、不重启），避免对永久故障无限重启空烧 token。
- **工具期静默**：`toolTimeoutSecs` 缺省 `0` = 工具执行期间**不检查**（长构建等合法长工具不会被打断）；设正数才在工具静默超限时恢复。

配置键（camelCase）见 `agent_dir()/extensions/hang-detect.json`：`timeoutSecs`（缺省 180，`0` = 关闭看门狗）、`toolTimeoutSecs`（缺省 0）、`maxRecoveries`（缺省 3，`0` = 不设上限）。`/hang-detect` 打开扩展设置面板（无参数；预设档位、Space/Enter 循环切换、立即落盘），`/hang-detect status` 打印运行状态与配置，`/hang-detect timeout|tool-timeout|recoveries <n>` 写入任意数值，`/hang-detect reset` 清运行状态。

### rewind

`rewind` 是编译期内置的**上下文回退**扩展（`Dev`/`Creator` 模式可用），**默认不启用**（需 `enabledExtensions` 或 `/extension` 面板开启）：误发送内容（错别字 / 发错对象 / 发错会话）时，把「最后一次用户输入 + 基于它产生的全部 assistant 与工具输出」从模型上下文中一起撤回。

- **机制**：`Session::rewind_last_user_input` 在活动分支上从叶子往回找**最近一条 user 消息**，把 `main` lane 的叶子指针移到该消息的父节点（root 时置 `None`）。该 user 消息与之后的所有条目一起离开活动分支，不再进入后续 provider 请求；**不删除、不改写历史**，原始日志仍在会话树上（`/tree` 可查看或切回），只追加一条 lane mutation。条目少了只影响分支指针，prompt 前缀缓存与历史透明度都不受影响。
- **回执**：TUI 经 `AgentCommand::Rewind` 下发，worker 调 `Agent::rewind_last_user_input`（移动叶子后立即用 `build_context_messages` 重建 `agent.messages`），回执 `RewindDone` 让 TUI 替换消息流并按选择回填/丢弃输入。
- **确认**：`/rewind` **每次都弹选择面板**（避免误操作）——`Rewind and restore input to the editor`（回退并回填输入框供修改重发）、`Rewind and discard the input`（回退并丢弃，历史仍可用 `/history` 找回）、`Cancel`。
- **忙碌拒绝**：run 进行中（流式输出 / 工具执行）时分支正被追加，回退会破坏进行中的回合，因此直接提示先按 Esc 中断；命令声明 `busy_safe`（handler 不锁 agent），忙碌时不会被当成普通文本排进 steer 队列。

### 扩展设置面板
占据输入框区域的设置视图（`ext_settings` 模块，对齐 `/settings`：打开时隐藏输入框）：**一次只展示一个扩展**的设置项，由该扩展自己的命令（如 `/loop-detect`）或 [`ExtensionUiRequest::ShowSettings { ext }`] 打开；`Space`/`Enter` 在预定义选项里循环切换（无自由输入，搜索框不捕获空格），顶部搜索框可直接输入过滤配置项（type to search），`Esc`/`Ctrl+C` 隐藏面板并把键盘交还编辑器；再次运行同一命令也会切换隐藏。变更经 [`Extension::apply_setting`] 回写，扩展自行决定是否落盘（loop-detect 立即写盘）。声明 [`Extension::settings`] 的扩展应自备一个打开面板的命令（或请求 `ShowSettings`）。选中项的配置说明行过长时按面板宽度自动折行完整展示（面板随折行行数撑高）。

### goal

`goal` 是编译期内置的自主目标扩展（`Dev`/`Creator` 模式可用），**默认不启用**（需 `enabledExtensions` 或 `/extension` 面板开启）。设目标后扩展在每轮 `agent_end` 自动续跑，直到模型完成、用户暂停/清除或 token 预算耗尽。fork 来源经 `fork_project()` 声明为 `pi-goal` v0.1.7（`/extension` 二级详情展示）。

**命令与工具**

- `/goal [--tokens 50k] <objective>`：创建/替换目标；`--tokens` 支持 `50k`/`2m` 后缀（亦可 `--tokens=2m`），非法或非正数报错。已有未完成目标时经 `Select` 面板二次确认「Replace / Cancel」后才替换。
- `/goal`（无参）：toggle 停靠面板中的目标段（对齐 plan-mode `/todos` 的三分支：面板未开 → 显示并打开；已开且可见 → 隐藏；已开但隐藏 → 重新显示）；无目标时提示用法。
- `/goal pause|resume|clear`：暂停 / 恢复 / 清除目标。`clear` 追加一次性停止指令并广播 `goal:changed`。
- 命令声明 `busy_safe = true`（handler 不锁 agent），流式输出期间也能立即 `/goal pause` 掐断自主续跑循环。
- 工具可见性：`create_goal` 恒可见；`get_goal`（读当前目标 JSON）/`update_goal`（只接受 `status="complete"`）仅在目标 `active` 时暴露，状态变化后请求 `RebuildTools` 下一轮生效。

**上下文注入（append-only，前缀缓存友好）**

`transform_context` 只做「追加」，**绝不改写/删除已发出的消息**——改写中间消息会让 prompt 前缀缓存从该点起全部失效。注入只在边界发生，事件经 `pending_injections` 队列排队、由下一轮 `transform_context` 取出 append：

- **激活契约**（`[[goal:active]]`）：目标进入 `active` 时追加一次的静态契约——objective（包在 `<untrusted_objective>` 中作为数据而非指令）+ 完成审计规则 + token 预算上限；**不含**实时用量数字（数字随轮次变化，放进一次性注入会破坏前缀缓存）。
- **状态换挡**（`[[goal:paused]]` / `[[goal:budget_limited]]` / `[[goal:complete]]`）：暂停/预算耗尽/完成时追加一条控制指令，历史里的旧契约保留。
- **清除指令**（`[[goal:cleared]]`）：`/goal clear` 后追加一次性停止指令（携带原 objective），注入后不再出现。
- **压缩补齐**：`active` 且上下文里已无 `[[goal:active]]` 标记（压缩把契约摘要掉）时补注入一次，仍是 append。

同一 run 的后续轮次不再注入（历史字节级不动），否则每轮贴一条「继续干活」会让 run 无法自然收尾。实时用量数字放在每轮新 append 的 `Continuation` 触发消息里（`continuation_nudge`：触发语 + 已用/预算/剩余 token 与耗时），同样不触碰历史。注入不落库，会话历史干净。

**续跑与记账**

- 自主续跑：`agent_end` 时目标仍 `active` 且无待处理输入 → 经 `Continuation` UI 请求触发下一轮（TUI 忙碌时跳过，避免抢跑用户输入；用户中断补发的合成 `agent_end`（`reason = "aborted"`）不续跑，等下一次自然回合结束再恢复）。
- 记账：`turn_start` 记起点、`turn_end` 按 `message.usage`（缺失 `total_tokens` 时兜底求和）累计 token 与耗时；超预算自动转 `budget_limited`，通知用户并追加收尾指令（当前回合内让模型总结进度、剩余工作与阻塞，不再开新实质工作）。

**停靠面板与重载保护**

- 停靠面板：`dock_lines()` 返回目标标题行（状态 + 用量）、截断的 objective、有预算时再一行 token 用量；可见性为内存态（`dock_shown`），不持久化。目标进入 `active`（创建/resume/恢复）时自动展示并请求打开面板。
- 持久化：目标状态经 `PersistSessionEntry` 写入会话自定义条目（合并写入，避免回声循环）；`on_session_start` / `on_session_switched` 从最近条目恢复。
- 重载保护：进程启动恢复时 `active` 目标自动转 `paused`（防静默恢复自主循环），提示 `/goal resume` 或 `/goal clear`；会话内切换（resume/fork/clone/import）不触发该保护。禁用扩展时清内存态（会话条目保留），重新启用需重启恢复。
- rich footer 经 `goal:changed` 事件显示目标标记 🎯。

### mcp

`mcp` 把 kiss 的 Model Context Protocol 客户端（`kiss-mcp`）移植为 prux 内置扩展（`Dev`/`Creator` 模式可用），**默认不启用**（需 `enabledExtensions` 或 `/extension` 面板开启）；fork 来源经 `fork_project()` 声明为 `kiss-mcp`（`/extension` 二级详情展示）。它不是 WebMCP（浏览器 CDP 集成），而是标准 MCP 客户端。**完整说明与各种 mcp.json 配置例子见 [extensions/mcp.md](extensions/mcp.md)。**

**配置发现**（6 个位置，全局优先、深度合并、后覆盖前；`//` 与 `/* */` 注释允许）：

| # | 路径 | 作用域 |
|---|------|--------|
| 1 | `~/.config/mcp/mcp.json` | 共享全局 |
| 2 | `~/.agents/mcp.json` | agents 全局 |
| 3 | `~/.agents/mcp/mcp.json` | agents 嵌套全局 |
| 4 | `agent_dir()/extensions/mcp/mcp.json` | prux 全局（`prux mcp --scope user` 写这里） |
| 5 | `<cwd>/.mcp.json` | 项目（需项目信任；`--scope project` 写这里） |
| 6 | `<cwd>/.prux/mcp.json` | 项目 prux（需项目信任） |

`mcpServers` 映射到 `ServerEntry`：stdio（`command`/`args`/`env`/`cwd`）或 HTTP（`url`/`headers`/`auth`/`bearerToken`/`bearerTokenEnv`/`oauth`），外加 `includeTools`/`excludeTools`（精确名过滤）、`disabled`、`lifecycle`（`persistent` 跳过空闲断开）、`idleTimeout`（分钟）、`requestTimeoutMs`。写入用原子 rename + 跨进程文件锁，权限 `0o600`；`command` 与 `url` 互斥、HTTP 认证不能用于 stdio。

**`mcp` 工具**（单个代理工具，9 个 action）：`status` 返回每服务器状态与计数；`list` 懒连接全部非禁用服务器并回填缓存；`search` 多词模糊匹配名称/描述；`describe` 返回单工具 schema；`call` 调用工具（Text 截断 100KB、Image 直传附件、Audio 转占位文本、`isError` 转工具错误）；`resources`/`read_resource`/`prompts`/`get_prompt` 对应资源与 prompt。工具以 `Parallel` 模式执行，每 RPC 通过取消令牌与父级 abort 联动、并有 `requestTimeoutMs`（默认 60s）超时。

**生命周期与缓存**：管理器懒连接（构造不启动子进程），空闲 `idleTimeout`（默认 10 分钟）后断开；tools/resources/prompts 元数据写入 `agent_dir()/extensions/mcp/cache.json`，以 `ServerEntry` 序列化的 SHA-256 指纹为 key，配置变化自动失效，`search`/`describe`/`/mcp status` 可免连接命中。

**OAuth 2.1**：authorization code + PKCE（S256）与 client credentials；凭据落 `agent_dir()/extensions/mcp/oauth.json`（按服务器名索引并绑定 URL，URL 变化即作废），token 刷新由 rmcp 的 `AuthorizationManager` 负责。

**命令**：`/mcp [status|list|tools [server]|test <server>|login <server>|logout <server>|reload]`（`login` 起后台任务打开浏览器并在 `127.0.0.1:3118/callback` 等回调，结果经聊天区系统消息回执）；CLI `prux mcp list|get|add|update|remove|enable|disable|test|login|logout`（`add`/`update` 支持 `--url` 或 `-- <command...>`，`--env`/`--header K=V`、`--auth`、`--bearer-token[-env]`、`--oauth-scope`/`--client-id`/`--client-secret`/`--client-credentials`/`--redirect-uri`/`--timeout-ms`；`login --no-browser` 在终端粘贴回调 URL，与 PKCE 状态同进程完成）。

### web-access

`web-access` 把 pi-web-access 的联网能力移植为 prux 内置扩展（`Dev`/`Creator` 模式可用），**默认不启用**（需 `enabledExtensions` 或 `/extension` 面板开启）；fork 来源经 `fork_project()` 声明为 `pi-web-access` v0.29.0（`/extension` 二级详情展示）。

**工具**

- `web_search`：`query`/`queries`（最多 3 条并发）、`numResults`、`recencyFilter`、`domainFilter`、`provider`（单个 / 数组 / `all`；`all` 不含 `duckduckgo`，它只能显式指定）、`includeContent`、`proxy`。provider 省略或 `auto` 时用配置默认值，否则按 `searchRouting.providers` 与偏好顺序（SearXNG → Exa → Brave → Tavily → Serper → Jina → Perplexity → DuckDuckGo）选择；单个 provider 失败按路由回退。每个查询各自返回合成答案与来源；失败以 `Error:` 文本就地返回（不中断整次调用）。
- `fetch_content`：`url`/`urls`（最多 3 个并发）、`mode`（`readable` 默认 / `raw`）、`proxy`。`readable` 用 html2text 把正文转为 markdown（去脚本/样式/导航，相对链接绝对化）；`raw` 原样返回文本响应体。单 URL 截断到 `maxInlineContentChars` 并提示 `get_search_content` 续读；多 URL 返回清单。
- `get_search_content`：按 `responseId` 取回 `web_search` 的结果或 `fetch_content` 的正文；`query`/`queryIndex`、`url`/`urlIndex`、`offset`/`limit` 切片，或 `findText`（`exact`/`case-insensitive`/`fuzzy`）定位段落。

**Provider 与配置**

本阶段实现 8 个纯 REST provider：`tavily` `brave` `exa` `jina` `searxng` `duckduckgo`（免密钥）`serper` `perplexity`。密钥可写在 `agent_dir()/extensions/web-search.json`（如 `tavilyApiKey` / `braveApiKey` / `exaApiKey` / `jinaApiKey` / `serperApiKey` / `perplexityApiKey`，支持 `$ENV_VAR` 间接引用），也可用同名环境变量（`TAVILY_API_KEY` 等）；SearXNG 配 `searxngBaseUrl`（可选 `searxngHeaders`）。`provider` 设默认 provider，`searchRouting.providers` 设路由顺序，`tools.{webSearch,fetchContent,getSearchContent}.enabled: false` 可按工具关闭，`maxInlineContentChars`（默认 30000）控制内联切片上限，`proxy`/`userAgent`/`fetch.timeout` 控制出口；`allowPrivateHosts` + `ssrfAllowRanges` 可放行内网地址（默认拒绝回环/私有/链路本地，逐跳校验重定向）。

**与上游的差异（Phase 1 有意收窄）**

尚未移植：浏览器 curator、Auto-Summary、`source_check`、GitHub/YouTube/PDF/本地视频/图片、`answer` 模式、浏览器 cookie 认证抓取、OpenAI/Parallel/TinyFish/Firecrawl 等 provider 与 `/websearch` 命令。对应参数传入时显式报错（不做静默降级）；`web_search` 无 curator，一律按 `workflow: "none"`；`includeContent` 改为同步抓取（上限 10 个 URL）。搜索/抓取数据存于进程内 LRU/TTL（128 条 / 1 小时），不写入会话 JSONL，跨进程重启不恢复。

### proxy

`proxy` 把 prux **已登录的 provider** 暴露成一个 OpenAI 兼容的本地网关（`Dev`/`Creator` 模式可用），**默认不启用**（需 `enabledExtensions` 或 `/extension` 面板开启）——它会占用端口、并把已存凭据变成 HTTP 端点，必须显式开启。

> 方向提醒：这是**入站**网关（客户端 → prux → provider）。**出站**代理（`HTTPS_PROXY` / `NO_PROXY`）见 [http-proxy.md](http-proxy.md)。

**命令**

| 命令 | 行为 |
|------|------|
| `/proxy listen <host:port>` | 设置并启动监听（持久化到 `agent_dir()/extensions/proxy.json`）；重设时**先绑定新地址、成功后才停旧监听**（绑定失败保留旧监听并报错）。裸命令只显示当前状态 |
| `/proxy provider <provider>` | 设置目标 provider（要求已知且有已存凭据，否则拒绝并提示 `/login`）；运行中即时生效，不需要重启监听 |
| `/proxy api-key <sk-xx>` | 设置入站 Bearer token（持久化到 `proxy.json` 的 `apiKey`）；**裸命令清空**配置值（关闭鉴权）。运行中即时生效，不需要重启监听 |
| `/proxy`（无参，三分支开关） | 面板未开 → 显示 dock 段并打开面板；已开且可见 → 隐藏本段；已开但隐藏 → 重新显示 |

扩展**启用时**若 `listen` 与 `provider` 都已配置，会按配置自动开监听；禁用扩展即停服（配置保留）。dock 段内容为 3 行统计（监听地址 · provider · 运行时长、连接数 + 请求数、token 累计 + 上下行流量与速率）加最近 5 条请求日志；速率是 5 秒滑动窗口（空闲回落 0），统计不落盘。

**端点**

| 方法 | 路径 | 说明 |
|------|------|------|
| `POST` | `/v1/chat/completions` | 支持 `stream: true/false`；请求体 `model` 字段选模型 |
| `GET` | `/v1/models`、`/v1/models/{id}` | 列出当前 provider 的可用模型（`owned_by` 为 provider 名） |
| `GET` | `/health` | 免鉴权探活 |

其余路径 `404`、已知路径方法不对 `405`，一律 OpenAI 错误体 `{"error":{message,type,param,code}}`。

**手工验证**：`examples/bot` 就是一个 OpenAI 兼容示例客户端（依赖走根包 dev-dependencies，随根包构建）：`RUST_LOG=debug cargo run --example bot` 会走 `/v1/chat/completions` 流式请求并把增量与完整回答打到日志上；网关地址 / 模型 / key 取 `BOT_DEMO_BASE_URL`（默认 `http://127.0.0.1:8765/v1`）/ `BOT_DEMO_MODEL` / `BOT_DEMO_API_KEY`。

**转发语义（协议翻译，不是字节透传）**

OpenAI 请求 → 内部 `AgentMessage` → `provider::stream_chat()`（自动走 prux 的 provider 解析、鉴权、OAuth 过期刷新、四协议适配与重试）→ 内部流事件回译成 OpenAI SSE chunk。因此 anthropic / google / github-copilot 等**非 OpenAI 协议**的 provider 同样可用。

**provider 私有请求头由网关代填**：部分 provider 要求客户端自报会话身份（opencode / opencode-go 缺 `x-opencode-session` 直接 `400 MissingSessionID`；anthropic 系用 `x-session-affinity` 做提示缓存亲和）。网关把「会话标识」交给 provider 层，由后者翻译成该 provider 真正需要的头，取值优先顺序为：**客户端入站头 `x-session-affinity` / `x-opencode-session`**（客户端自己就懂会话时用它的，绝不拿派生值盖真实会话）→ 否则网关按请求里的**会话稳定前缀**（系统提示 + 首条消息）派生一个稳定 id。派生值在同一会话逐轮追加历史时不变（路由与提示缓存才有效），换会话才变。两种情况下客户端都不必为某个 provider 改代码。

网关自己在**出站**方向上仍遵循常规代理环境变量（`HTTPS_PROXY` / `NO_PROXY` 等，见 [http-proxy.md](http-proxy.md)）——它是 provider 层共用的 HTTP 客户端行为，不因走网关而改变。

**模型选择**

`/proxy provider` 只锁 provider，**模型 id 由请求体 `model` 字段逐请求给出**，且必须在该 provider 的目录里**精确命中**（id 或 name 相等；`find_model` 的第三档前缀回退被拒绝，避免静默换模型）。缺失 → `400`；找不到 → `404 model_not_found`（错误信息里给出若干可用 id）。响应体回显**规范 id**（目录里的 id，而非请求原文）。

**参数支持面**

| 参数 | 处理 |
|------|------|
| `model` / `messages` / `stream` | 映射（`system`/`developer` 消息拼进系统提示；`tool` 消息转工具结果） |
| `temperature`、`max_tokens`/`max_completion_tokens` | 映射（两个 token 字段同时给出且不等 → `400`） |
| `reasoning_effort` | 映射为 prux thinking 级别（`none` → `off`），经模型能力钳制 |
| `top_p` | 经 `on_payload` 注入上游请求体顶层（openai / anthropic / responses 系协议有效）；google 系协议无法表达 → `400` |
| `tools` / `tool_choice` | 映射；`tool_choice` 只接受 `auto` / `none`（`required` 与指定函数无法跨协议如实表达 → `400`）；工具 `strict: true` → `400` |
| `stream_options.include_usage` | 支持：倒数第二帧带 `usage` |
| `stop`、`n>1`、`logprobs`、`top_logprobs`、`logit_bias`、`seed`、`response_format`（非 `text`）、`modalities`、`audio`、`prediction`、非 0 的 `frequency_penalty`/`presence_penalty`、`web_search_options`、`reasoning`、`verbosity` | **`400 unsupported_parameter`**（语义会变但不能如实表达 → 显式拒绝，不静默降级） |
| `user`、`metadata`、`store`、`service_tier`、`parallel_tool_calls`，以及未识别字段 | 忽略（纯元数据不影响结果） |

**映射细节（客户端可见的协议差异）**

- 思考内容走 `delta.reasoning_content`（非流式 `message.reasoning_content`）；思考帧**不带** `content` 键。
- assistant 消息里的 `reasoning_content` 会被保留为思考块回传给上游（DeepSeek 等要求回传，否则会 400）。
- 工具调用以**整块 delta** 下发：prux 内部事件直到 `ToolCallEnd` 才揭示 `id`/`name`，而 OpenAI SSE 要求 `id`/`function.name` 出现在该 `index` 的首个 delta 里，故参数先缓冲、再一次性发出（`arguments` 为完整 JSON 字符串，协议上合法）。多个工具调用按出现顺序递增 `index`。
- 图片只接受 **data URI**（`data:image/png;base64,...`）：远程 `http(s)` 图片 URL 不代抓（避免 SSRF 面与内存放大），直接 `400`；目标模型 `input` 不含 `image` 时也 `400`（而不是让上游静默替换成占位文本）。
- 非流式请求内部照常走流式调用，聚合后一次性返回 `chat.completion`；上游未回传 usage 时按 0 计。

**鉴权 / 并发 / 错误**

- **鉴权**：`proxy.json` 的 `apiKey` 或环境变量 `PRUX_PROXY_TOKEN`（环境变量优先）；未配置则免鉴权。配置后须带 `Authorization: Bearer <token>`（常量时间比较），否则 `401`；**鉴权先于路由**（未带 token 时不会透露路径是否存在，也只有 `/health` 免鉴权）。
- **并发**：在途请求上限 30（网关与 TUI 同进程，必须防内存被吃干），超出立即 `429` + `Retry-After`（客户端本就实现退避）；请求体上限 8 MiB；客户端读得太慢导致未写出帧超过 1 MiB 时中止该流（错误帧收尾）。
- **错误**：网关自身校验失败 → `400`/`404`；上游失败且**首个 chunk 之前**发现 → **透传上游状态码**（`429` 尤其重要），上游 5xx/网络错误 → `502`，超时 → `504`；已发出 chunk 之后失败 → SSE `{"error"}` 帧 + `[DONE]` 收尾（HTTP 状态已定，无法回改）。
- **断连**：客户端断开 → 立即丢弃上游流（不继续消耗 token），该请求在面板里记为 `499`。

**配置（`agent_dir()/extensions/proxy.json`）**

```json
{
  "listen": "127.0.0.1:8765",
  "provider": "deepseek",
  "apiKey": ""
}
```

`listen` 接受 `127.0.0.1:8765` / `0.0.0.0:8765` / `localhost:8765` / `[::1]:8765`；不接受裸端口与端口 0。`apiKey` 留空即不鉴权（`PRUX_PROXY_TOKEN` 仍会覆盖它）。键名 camelCase。dock 面板第 1 行会显示**生效中**的 token（首尾各 4 个字符、中间 5 个 `*`；带 `(env)` 表示来自 `PRUX_PROXY_TOKEN`，未配置则显示 `(none)`）；短于 12 个字符的 token 整体打码为 `*****`（否则露出的比遮住的还多），裸 `/proxy` 的状态提示只说有没有鉴权、不显示值。

**有意收窄（本阶段不做）**

`/v1/completions`（legacy）、`/v1/responses`、embeddings、**CORS**（不给跨站头 = 不把额度开放给任意网页；浏览器端客户端需自行代理）、远程图片抓取、并发排队、扩展设置面板、CLI flag、`stop`/`n>1`/`logprobs`/`response_format`/`seed`/惩罚项。

### jet

`jet` 是编译期内置的**虚拟模型路由器**（移植自 pi 的示例插件 `examples/extensions/jev-router.ts`；`Dev`/`Creator` 模式可用）。它注册虚拟模型 `jet/auto`，让规划阶段跑在强模型上、实现阶段跑在便宜模型上。**默认不启用**，需在 `/extension` 面板开启。

- **路由语义**（对齐 pi 的示例插件）：
  - 规划阶段用 `planning` 档；配了 `planningComplex` 档且分类器把题面判为复杂（`complex` 概率 ≥ 0.5）时改用后者。
  - 本轮出现第一次**成功**的 `edit`/`write` 工具结果后，同一轮的下一个请求起切到 `implementation` 档，此后整个会话留在那里。
  - `reason == direct`（压缩摘要、扩展直调）直接给 `implementation` 档。
  - `request.previous` 命中的就是本配置里的某个规划档（provider + 模型都一致）时直接沿用，跳过分类器——省一次分类调用与一次 prompt-cache miss。
  - 分类器未配置 / 调用失败 / 答案格式不符一律回退 `planning` 档，**不报错**。
  - 相位（`{phase, model}`）作为**路由器状态**落成会话分支上的 custom 条目，跟着会话树走、也能扛压缩。
- **配置** `agent_dir()/extensions/jet.json`（**没有默认值**，空串 = 未配置）：

```json
{
  "classifier": { "provider": "openrouter", "model": "~typesafe/jev-latest" },
  "router": {
    "planning": { "provider": "openai", "model": "gpt-5.6-terra" },
    "planningComplex": { "provider": "anthropic", "model": "claude-opus-4-8" },
    "implementation": { "provider": "opencode", "model": "kimi-k3" }
  }
}
```

三档各自带 **provider + 模型 id**，可以来自不同 provider（分类器也是独立的一份）。`classifier` 整体可缺（缺了就只按普通规划档跑）；`router.planningComplex` 可缺（缺了分类器一定不参与）。只有 `router.planning` 与 `router.implementation` 两档都配置完整、且每档都能在模型目录里解析到（是 chat 条目、不是图片/分类器条目、也不是虚拟模型自己）时才注册 `jet/auto`；否则连 `jet` 这个 provider 都不会出现在 `/model` 里。

`jet/auto` 首次响应前展示的上下文窗口 / 输出上限取 `planning` 与 `planningComplex` 两档中**较小**者；thinking 档位取三个目标模型能力的**交集**（跨 provider 时就按各家的实际能力取，空交集只有 `off`）。用户选中的档位原样透传给目标模型，由宿主按目标能力钳制。

- **命令**：
  - `/jet` —— 打印每档的 `provider/model` 与注册状态（含是哪一档解析不到）。
  - `/jet set <provider> <model-id>` —— 配置分类器模型，必须是 `type: classifier` 条目（如 `/jet set openrouter ~typesafe/jev-latest`）。
  - `/jet router [--provider <p>] [--planning <id>] [--planning-complex <id>] [--implementation <id>] [--planning-provider <p>] [--planning-complex-provider <p>] [--implementation-provider <p>]` —— **增量**覆盖：只改给到的项，其余保持现值（`--flag=value` 也接受）。
    - `--provider` 一次给三档设同一个 provider（常规用法）；
    - `--*-provider` 单独覆盖某一档，用来让三档分属不同 provider；
    - 两者同时给时以单项为准。
  - `/jet reset` —— 清空全部配置；若当前会话正选中 `jet/auto`，自动切回最近一次路由到的物理模型（拿不到就回落到 provider/model 解析的兜底模型），避免留下必然报错的选中项。

  写命令都先校验（未知 flag、未知 provider、目录里没有的模型、类型不对都拒绝且不落盘），写盘成功后**立即重注册**，不需要重启；然后按 `/model` 面板选中 `jet/auto` 即可。示例：

```text
/jet router --provider openai --planning gpt-5.6-terra --implementation gpt-5.6-luna
/jet router --planning-complex-provider anthropic --planning-complex claude-opus-4-8
/jet router --implementation-provider opencode --implementation kimi-k3
/jet set openrouter ~typesafe/jev-latest
```

- **成本口径**：底栏与 `/session` 显示的是路由后**物理模型**的开销，footer 也带 `→ <物理模型>` 后缀；分类器调用的 token 用量**不计入**会话成本（与 pi 一致）。
- **依赖**：各档目标 provider 的凭据、分类器模型的凭据。

## 编写一个扩展

一个最小工具扩展：实现 `Extension` trait，提供工具与钩子，再注册。

```rust
use crate::core::extensions::{
    Extension, ExtensionHook, ExtensionTool, ToolExecCtx, ToolResult,
};
use crate::core::tools::{ToolError, ToolResult as TR};
use serde_json::Value;

struct Echo;

impl Extension for Echo {
    fn name(&self) -> &str { "echo" }
    fn description(&self) -> &str { "Echo the input" }

    // 工具定义并入模型可见的工具列表
    fn tools(&self) -> Vec<ExtensionTool> {
        vec![ExtensionTool::simple(
            "echo",
            "echo the input",
            serde_json::json!({ "type": "object" }),
            "echo the input",
        )]
    }

    // 声明关心的钩子
    fn hooks(&self) -> Vec<ExtensionHook> {
        vec![ExtensionHook::BeforeToolCall, ExtensionHook::AfterToolCall]
    }

    // 按名字分发工具执行（同步）
    fn execute_tool(&self, name: &str, args: &Value) -> Result<ToolResult, ToolError> {
        if name != "echo" {
            return Err(ToolError("no such tool".into()));
        }
        Ok(ToolResult::text(
            args.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        ))
    }

    // 异步执行（子代理等需要异步能力时覆写）；默认桥接同步 execute_tool
    fn execute_tool_async(
        &self,
        name: String,
        args: Value,
        _ctx: ToolExecCtx,
    ) -> BoxFuture<'static, Result<ToolResult, ToolError>> { /* ... */ }

    // 工具调用前拦截：Err 阻止，Ok(Some(v)) 改写参数，Ok(None) 放行
    fn before_tool_call(&self, _name: &str, _args: &Value)
        -> Result<Option<Value>, ToolError> { Ok(None) }

    // 工具调用后处理：改写结果文本
    fn after_tool_call(&self, _name: &str, _args: &Value, result: &str)
        -> Result<String, ToolError> { Ok(result.to_string()) }
}

// 注册（应用入口处）
core::extensions::register_extension(Echo);
```

## Extension trait 方法参考

| 方法 | 说明 |
|------|------|
| `name()` / `description()` | 扩展名与描述（/extension 面板详情展示） |
| `fork_project()` | 扩展来源（移植自哪个插件及其版本/URL）；`/extension` 详情展示，`None` = 原创 |
| `default_enabled()` | 是否默认启用；返回 `false` 的扩展注册后为禁用态，需 `enabledExtensions` 或 `/extension` 面板开启 |
| `modes()` | 所属扩展模式（可多个；默认只属全部模式） |
| `tools()` | 提供的扩展工具定义（并入模型可见工具列表） |
| `virtual_models()` | 提供的虚拟模型定义（并入模型目录；chat 型每次请求路由到物理模型，`operation()` 构造的 image / classifier 型只做目录条目，可经 `with_image_impl()` / `with_classifier_impl()` 随条目带上实现）。见 [virtual-models.md](virtual-models.md) |
| `suppress_documentation()` | 返回 `true` 时从**默认**系统提示词中移除整段程序文档描述（自定义系统提示词不受影响）；`doc-helper` 用它把帮助改为按需询问 |
| `mcp_servers()` | 以会话为作用域注册 MCP 服务器（不落盘，每次加载 MCP 配置时调用）；`mcp.json` 里的同名条目优先 |
| `image_apis()` / `classifier_apis()` | 无自注册条目的图片 / 分类器协议实现（`api` 名 → 实现）：用于覆盖同名内置实现，或给用户 `models.json` 条目供货。自注册条目请用 `with_image_impl()` / `with_classifier_impl()`。见下方「协议实现（图片与分类器）」 |
| `commands()` | 斜杠命令声明（如 `/plan`、`/todos`）；随扩展生命周期动态增删；`ExtensionCommand.subcommands` 声明 `/<命令> ` 后的二级候选 |
| `cli_flags()` / `apply_cli_flag()` / `apply_cli_flag_value()` | 声明/消费命令行 flag（如 `--plan`）；仅已启用扩展的 flag 注入解析，`takes_value` 的 flag（如 `--subagents-workflow-file <path>`）走 `apply_cli_flag_value` |
| `keybindings()` | 全局快捷键声明（TUI 键盘分发兜底遍历，如 `plan-mode.toggle` = `alt+p`） |
| `hooks()` | 关心的事件钩子（决定分发时哪些方法被调用） |
| `filter_tools()` | 运行时过滤模型可见的**内置**工具（plan-mode 用它移除 `edit`/`write`） |
| `filter_extension_tools()` | 运行时过滤**本扩展自己**的工具（显隐变化后需触发 `RebuildTools` 才在下一轮生效） |
| `hosts_scripts()` | 是否承载脚本执行（`codemode`）：它调起的嵌套工具视为「脚本调用」 |
| `prepare_loadout()` | 按当前工具集改写**模型看到的工具声明**（如把可调工具声明内联进自己的 description，或在 `only` 模式下摘掉声明） |
| `transform_context()` | LLM 调用前修改上下文 |
| `before_tool_call()` / `before_tool_call_ext()` | 工具调用前拦截（可 block/改写参数/terminate） |
| `after_tool_call()` / `after_tool_call_ext()` | 工具调用后处理（可改写文本/替换 content/details/isError/usage） |
| `on_agent_event()` | 接收 agent 内核事件（`agent_start` / `turn_end` / `agent_end` / `tool_execution_*` / 自定义事件） |
| `on_enabled_changed()` | 启用状态变化回调（注册时、/extension 面板切换后） |
| `on_registered()` | 注册时接线需要 TUI 状态的执行入口（命令/快捷键 handler） |
| `on_session_start()` | 会话加载/恢复钩子（cwd 校验、resume 重建） |
| `on_session_switched()` | 会话切换钩子（新会话路径 + 消息列表） |
| `on_before_compact()` / `custom_summarize()` / `on_compact_failure()` | 压缩钩子（见 [compaction.md](compaction.md)） |
| `on_ui_choice()` | 面板选择结果回传（Enter/Esc 按 id 分发；返回 `Some(text)` 触发下一轮 prompt） |
| `on_user_prompt()` / `on_user_prompt_with_context()` | 用户 Enter 提交前的输入拦截（可改写文本或认领）；`_with_context` 版额外拿到当前对话与 busy 标志 |
| `on_user_submit()` | 用户提交的广播通知（无动作，声明 `UserPrompt` 钩子的扩展都收到） |
| `on_exec_ctx()` | 会话建立 / 每轮开始 / 会话切换后拿到当前 `ToolExecCtx`（工具执行之外也能用 `make_sub_agent`） |
| `on_boundary()` | 轮收尾 / 结算边界（`turn_end`、`agent_before_settle`）：可要求续跑、优雅结束或追加投影草稿 |
| `provider_stream_event()` | 归一化**之前**的 provider 原始流事件（最高频钩子之一，实现只应缓冲、不做 IO） |
| `cache_warming_decision()` | 覆盖提示缓存保温决策（返回 `"warm"` / `"stop"`，最后一个给出 action 的扩展生效） |
| `on_extension_event()` | 扩展↔扩展事件总线订阅（订阅 = 声明 `ExtensionEvent` 钩子） |
| `suggestions()` | 输入框候选来源（如 `@` 提及） |
| `settings()` / `apply_setting()` | 声明扩展设置项 / 应用扩展设置面板的变更（见下方「扩展设置面板」） |
| `wants_redraw()` | 请求 TUI 持续重绘（后台任务跑动时让 dock 里的 spinner/计数动起来） |
| `render_custom_message()` | 渲染会话自定义条目（`CustomMessage` 请求）；同一份 `data` 会实时与会话恢复时各调一次，实现须是纯函数 |
| `overlay_view()` / `on_overlay_event()` | 覆盖层内容拉取与按键回传（每帧；须只读自身状态、不得加 agent 锁） |
| `transform_context_with_system()` | `TransformContext` 之后、每次 LLM 调用前变换**含 system 消息**的完整 transcript |
| `execute_tool()` / `execute_tool_async()` | 扩展工具的同步 / 异步执行入口（默认分发到 `tools()` 声明的工具） |
| `dock_lines()` | 停靠面板内容行（状态栏上方的可滚动面板；空列表 = 无内容，面板自动隐藏） |
| `tool_renderers()` | 为工具（典型是 MCP 聚合工具 `mcp`）提供自定义渲染器。见下方「工具渲染器」 |

## 事件钩子（ExtensionHook）

`hooks()` 声明后，对应 trait 方法才被分发调用（共 13 个钩子）：

- **`TransformContext`** — LLM 调用前修改消息（`transform_context`）
- **`ContextWithSystem`** — `TransformContext` 之后、每次 LLM 调用前拿到含 system 的完整 transcript（`transform_context_with_system`）
- **`BeforeToolCall`** / **`AfterToolCall`** — 工具调用前拦截 / 后处理（`*_ext` 增强版）
- **`AgentEvent`** — agent 内核事件流入（`on_agent_event`，高频面）
- **`Dock`** — 停靠面板内容采集（`dock_lines`，TUI 每帧采集）
- **`Overlay`** — 覆盖层内容与按键（`overlay_view` / `on_overlay_event`，每帧采集）
- **`UserPrompt`** — 用户 Enter 提交前的输入拦截（`on_user_prompt` / `on_user_prompt_with_context`）与提交广播（`on_user_submit`）
- **`ExtensionEvent`** — 扩展↔扩展事件总线订阅（`on_extension_event`；订阅 = 声明本钩子）
- **`Suggestions`** — 输入框 `@` 候选来源（`suggestions`，未声明不被轮询）
- **`Boundary`** — 轮收尾 / 结算边界（`on_boundary`，可要求续跑/结束/追加投影草稿）
- **`CacheWarmingDecision`** — 提示缓存保温决策覆盖（`cache_warming_decision`）
- **`ProviderStreamEvent`** — 归一化之前的供应商原始流事件（`provider_stream_event`，最高频面）

未声明某钩子的扩展在该面上完全不被分发：高频面（`AgentEvent` / `ProviderStreamEvent` / `Dock` / `Overlay` / `Suggestions`）靠这个跳过。

增强接口（`*_ext`）提供 block / terminate / content / details / isError / usage 语义；默认桥接旧接口，新扩展可直接覆写。

## 扩展工具（ExtensionTool）

`ExtensionTool` 结构：`name` / `description` / `label` / `namespace` / `parameters`（JSON Schema）/ `output_schema` / `snippet` / `prompt_guidelines` / `constrained_sampling` / `grammar_sampling` / `exposure`（`ToolExposure::Deferred` 表示不进模型可见列表，需 `tool_search` 激活）/ `annotations` / `render_shell` / `execution_mode` / `prepare_arguments`。可用 `ExtensionTool::simple(name, desc, params, snippet)` 快捷构造，其余字段取默认。

工具执行上下文 `ToolExecCtx` 提供 `cwd`、`make_sub_agent`（按 `SubAgentSpec` 物化一个状态隔离的子代理）、`parent_abort`，以及当前 agent 的身份（`agent_id` / `depth` / `parent_model`）。`subagent` 扩展的 `Agent` 工具即通过它驱动独立子代理（详见 [extensions/subagent.md](extensions/subagent.md)）。

## 协议实现（图片与分类器）

内置的图片生成 / 分类器协议只有 `openrouter-images` 与 `typesafe-system-one`；
扩展可以注册自己的 `api` 名（对齐 pi 的 `registerProvider({ models, images, classifiers })`：
条目与实现一起给出，`api` 名只写一遍）：

```rust
use prux::core::{
    extensions::{Extension, ExtensionTool},
    provider::{ClassifierContext, ClassifierResult, ModelConfig, ModelType},
    provider::api_impls::ClassifierApiImpl,
    virtual_models::VirtualModelDefinition,
};
use std::sync::Arc;

struct MyClassifier;

impl ClassifierApiImpl for MyClassifier {
    fn classify<'a>(&'a self, model: &'a ModelConfig, context: &'a ClassifierContext)
        -> BoxFuture<'a, prux::error::Result<ClassifierResult>>
    {
        Box::pin(async move { /* 发请求 → 组装 ClassifierResult */ })
    }
}

impl Extension for MyExtension {
    fn name(&self) -> &'static str { "my-classifier" }
    fn tools(&self) -> Vec<ExtensionTool> { Vec::new() }

    /// 目录条目：provider / id / 展示名 / 类型 / api 名；实现挂在同一条目上
    fn virtual_models(&self) -> Vec<VirtualModelDefinition> {
        vec![VirtualModelDefinition::operation(
            "my-provider", "my-classifier", "My Classifier", ModelType::Classifier, "my-system-one",
        )
        .with_classifier_impl(Arc::new(MyClassifier))]
    }
}
```

- `api` 名在 `operation()` 里只写一遍：注册时实现就按该名进分发表，不会出现条目与实现名字对不上的情况。
- `with_image_impl()` 同款，只是实现 trait 是 `ImageApiImpl`（`generate` 返回 `AssistantImages`）。
- 条目也可以不带实现，由 `image_apis()` / `classifier_apis()` 单独给实现：
  用于**覆盖同名内置实现**，或让用户把条目写进 `models.json`（声明 `"type": "image"` /
  `"type": "classifier"` 与同名 `"api"`，见 [models.md](models.md)）。
- 分发顺序：**扩展注册的实现优先，其次内置实现**（同名 `api` 由扩展覆盖内置）；
  都没有时返回「协议未实现」错误（`generate_images` / `classify` 的“不抛错”契约不变）。
- 实现随扩展启停过滤：扩展被 `/extension` 面板禁用（或 `--no-extensions`）后立即不再参与分发。
- 非 chat 条目请显式写 `api`：image / classifier 型不从 provider 级 `api` 继承（与 pi 一致）。

## 工具渲染器（`tool_renderers()`）

宿主内置渲染按工具名分派（`edit` 画 diff、`bash` 画 `$ cmd`、`mcp` 折叠 5 行…），
扩展可以用 `tool_renderers()` 接管某个工具的工具块渲染：

```rust
use crate::core::extensions::{RegisteredToolRenderer, RichSpan, ToolRender, ToolRenderCtx, ToolRenderer};
use std::sync::Arc;

struct MyMcpRenderer;

impl ToolRenderer for MyMcpRenderer {
    fn name(&self) -> &'static str { "my-mcp" }
    /// 接管的工具名（内置或扩展工具都可以）
    fn tools(&self) -> &[&'static str] { &["mcp"] }
    /// 同步渲染：返回 None 表示这次不接管（交回内置渲染）
    fn render(&self, ctx: &ToolRenderCtx<'_>) -> Option<ToolRender> {
        let (text, is_error) = ctx.result?;   // 工具还在执行时为 None
        let color = if is_error { "error" } else { "success" };
        Some(ToolRender {
            lines: vec![
                vec![RichSpan::fg(color, format!("{} {}", ctx.tool, text.lines().next().unwrap_or("")))],
                vec![RichSpan::plain(format!("{}ms", ctx.duration_ms.unwrap_or(0)))],
            ],
            // 折叠（未 Ctrl+O）时保留的视觉行数；None = 不折叠
            preview_lines: Some(2),
        })
    }
}

impl Extension for MyExtension {
    // …
    fn tool_renderers(&self) -> Vec<RegisteredToolRenderer> {
        vec![RegisteredToolRenderer::new(Arc::new(MyMcpRenderer))]
    }
}
```

- **上下文** `ToolRenderCtx`：`tool` / `args`（原始 JSON）/ `result`（`Option<(&str, bool)>`，
  `None` = 仍在执行）/ `duration_ms` / `expanded`（Ctrl+O）/ `width` / `inner_width`（已扣 padding）。
  拿不到 agent 或工具执行句柄。
- **产出** `ToolRender`：`lines`（`Vec<Vec<RichSpan>>`，`RichSpan.fg` 为主题键或 hex，`None` = toolOutput 色）
  与 `preview_lines`。宿主负责铺背景、按宽度折行（视觉行）与裁剪；裁剪提示行由宿主补。
- **实现约束**：必须同步、无 I/O、不阻塞（每个工具块每次重渲染都调一次），不要在渲染里发工具调用或改状态。
- **生效范围**：同一工具名以**最后一次注册**为准（与 `image_apis()` 同口径）；宿主的 `edit` 内置 diff
  渲染不参与接管；扩展被 `/extension` 面板禁用（或 `--no-extensions`）后立即回退内置渲染，
  渲染缓存按渲染器指纹失效，不需要手动清屏。
- **直接注册**（不走 `Extension` 声明的场景）：`core::extensions::register_tool_renderer(Arc::new(r))`。

## 命令 / 快捷键 / CLI flag

- **斜杠命令**：`commands()` 声明「存在性」，`on_registered()` 里用 `register_slash_command` 接线执行入口。`busy_safe` 表示命令在流式输出时能否安全执行（仅 handler 完全不锁 agent 的只读命令置 `true`，如 `/todos`；需读写 agent 的如 `/plan` 置 `false`）。
- **子命令提示**：`ExtensionCommand.subcommands`（`SubcommandDef { name, description }` 列表）声明二级候选。用户输入 `/<命令> ` 后输入框弹出子命令面板，空格后的第一个词在面板内模糊过滤；Tab 只补全（`/goal pause `），Enter 补全并立即执行（与 `/` 命令面板同语义）。出现第二个空白（已在写自由参数）或查询无匹配即关闭面板。声明顺序 = 候选顺序（空查询时默认选中第一项）；清单只是候选元数据，解析仍在命令 handler 里，两者需保持一致（建议用同一个 `const` 声明并配一致性测试）。
- **快捷键**：`keybindings()` 声明（如 `plan-mode.toggle` → `alt+p`），`on_registered()` 里用 `register_extension_keybinding` 接线。不进 keybindings.json 定制体系（见 [keybindings.md](keybindings.md)）；内置快捷键优先，扩展快捷键由 TUI 键盘分发兜底遍历，busy 态跳过整段分发。
- **CLI flag**：`cli_flags()` 声明开关（如 `--plan`），仅已启用扩展注入 clap 解析，`apply_cli_flag` 回传；声明 `takes_value: true` 的会收一个值（如 `--subagents-workflow-file <path>`）并走 `apply_cli_flag_value`。未启用扩展的 flag 不注入，`--xxx` 按未知参数报错。

## dock 扩展

实现 `dock_lines() -> Vec<DockLine>` 向停靠面板（状态栏上方）提供常显内容。`DockLine` = `Vec<DockSpan>`（主题键名 + 缺省色，`DockSpan::plain` 为终端默认前景）。返回空列表表示无内容，面板自动隐藏。每段可见性由扩展自己控制（如 plan-mode 经 `/todos` 或 `ShowDock` 请求决定）；首行为扩展自带的标题行。面板开关由用户 `/dock` 切换（见 [tui.md](tui.md)）。

扩展可以在"有内容要展示"时经 `ShowDock` 请求自动打开面板（`plan-mode` 进入执行、`goal` 激活、子代理派发、工作流开跑都如此），无需用户手动 `/dock`；同一时刻只排一条请求。

用户提交输入时，核心还会广播一次提交通知（声明 `UserPrompt` 的扩展的 `on_user_submit`）——与输入拦截不同，它不短路、不看谁认领。子代理扩展借此在用户开新回合（且已无在跑的代理/工作流）时把上一轮已完成的记录从 dock 隐去，使面板不再占位；记录本身保留（`/agents` 面板 / `@handle` resume 不受影响）。

## footer 扩展

实现 `FooterExtension`：`on_agent_event` 收 agent 内核事件，`render(&FooterCtx) -> Vec<FooterLine>` 出多行带样式文本。`FooterCtx` 提供 `width`、`cwd`、`model`、`thinking_level`、`git_branch`、`context_percent`、`context_window`、`busy`、`messages` 等渲染上下文。`FooterLine` = `Vec<FooterSpan>`（主题键名 + 缺省色，渲染时经 `Theme::style` 着色）。

footer 扩展**互斥**：注册/面板切换时启用一个会自动禁用其他 footer，保证至多一个生效；全部禁用后底栏不显示。

## banner 扩展

实现 `BannerExtension`：`render(&BannerCtx) -> Vec<BannerLine>` 出多行带样式文本。`BannerCtx` 提供 `width`（横幅可用宽度）与 `height`（终端总高）；`BannerLine` = `Vec<BannerSpan>`（主题键名 + 缺省色，渲染时经 `Theme::style` 着色）。返回空行列表 = 不显示。

banner 只显示一次：**干净启动**（扩展已启用、会话无历史消息、无启动初始消息、用户尚未提交输入）时渲染在窗口顶部、消息区之上；用户首次提交输入（`tui.input.submit`）后本进程内永久隐藏（会话切换也不重现），隐藏后不占布局（消息区让位）。内置 `banner` 只声明 `Minimal`（见下文布局宽度的三档）。

布局：art 左对齐，顶部/底部各 2 行空行（不画竖线）、左侧 4 列 padding；竖线只画在 6 个 art 行、与 art 间隔 4 列、以 art 最长行为基准对齐（短行间隔自动补足），竖线与右侧信息区间隔 1 列。宽度三档——宽 ≥ art宽(33)+左padding(4)+间隔(4)+竖线(1)+信息间隔(1)+信息区最小宽(30) = **73** 时渲染 art + 竖线 + 6 行信息区（prux 版本 / 描述 / 常用工具 / `/extension` 提示 / 快捷键提示，与 art 行对齐、上下各留 2 空行）；**37–73** 只渲染 art（无竖线无信息区）；**< 37** 整块隐藏。终端高度不足（放不下 art + 4 行 padding + 消息区 + 状态栏 + 输入区）时同样隐藏。内置 `banner` 只声明 `Minimal`（极简及以上可用）。

banner 扩展**互斥**：注册/面板切换时启用一个会自动禁用其他 banner，保证至多一个生效；全部禁用后不渲染。

## /extension 面板

`/extension` 打开扩展选择面板（对齐 /model 面板布局）：

- **空格**：切换选中扩展的启用状态，立即生效 + 写盘；关闭面板后重建 Agent 工具列表与系统提示
- **回车**：进入二级详情（描述、mode、tools、hooks、commands、类型）
- **Tab**：切换扩展模式（`all` → `dev` → `creator` → `minimal` → `all`），写盘持久化
- **Esc**：关闭面板并应用变更（`RebuildTools`）

极简模式下非本模式扩展不显示。默认可选（`default_enabled() == false`）的扩展共 15 个：`plan-mode`、`goal`、`web-access`、`proxy`、`doc-helper`、`subagent`、`loop-detect`、`hang-detect`、`notify`、`rewind`、`codemode`、`mcp`、`jet`、`tasks`、`debug-provider`；它们默认未勾选（关闭），空格开启后立即生效并写入 `enabledExtensions`。

## settings 与启用状态

- **`disabledExtensions`**：settings.json 中禁用的扩展名数组。注册时据此恢复初始状态（对默认启用的扩展 = 关闭）；/extension 面板切换后写回**全局**。项目 `.prux/settings.json` 里的同名字段与全局取并集（只在本项目禁用某内置扩展；未受信任的项目不生效）。
- **`enabledExtensions`**：settings.json 中显式启用的扩展名数组，仅对声明 [`default_enabled() == false`] 的默认可选扩展（`plan-mode`、`goal`、`web-access`、`proxy`、`doc-helper`、`subagent`、`loop-detect`、`hang-detect`、`notify`、`rewind`、`codemode`、`mcp`、`jet`、`tasks`、`debug-provider`）有意义。这些扩展注册后默认禁用，必须在此列出（或经 `/extension` 面板开启）才参与钩子/工具/命令分发；关闭时从列表移除（回到默认态）。项目层同样与全局取并集（项目可自行开启默认关闭的扩展）；两层冲突时禁用优先（与 `disabledExtensions` 的并集语义一致）。
- **`extensionMode`**：`"minimal"`、`"dev"`、`"creator"`、`"all"`（默认，兜底全集）。`All` 是兜底模式，所有扩展隐式属于它；扩展声明的模式（如 `Minimal`）表示“该级别及以上可用”，列出多个（如 `[Dev, Creator]`）则覆盖两个并排模式。是运行时过滤层，不触碰各扩展 enabled 状态；切回全部模式即恢复原状。

settings 相关键见 [settings.md](settings.md)。修改 settings 后重启生效；`/reload` 会重新接线扩展并重建工具列表与系统提示，但无法加载新编译的扩展代码。

## UI 请求总线

扩展经 `request_ui` 提交 UI 请求，TUI 事件循环每轮取一个处理：

- **`Select`**：弹出通用自定义选择面板，Enter/Esc 结果按请求 id 经 `on_ui_choice` 回传（返回 `Some(text)` 触发下一轮 prompt）
- **`Notify`** / **`NotifyRich`**：聊天区系统消息（info / success / error / warning）
- **`CustomMessage { label, custom_type, data }`**：聊天区自定义卡片，TUI 按 `custom_type` 经 `render_custom_message` 反查渲染；是否落盘由扩展自己决定（落盘后经 `on_session_switched` 重放）
- **`ShowOverlay { id }`** / **`HideOverlay { id }`**：打开 / 关闭扩展覆盖层（内容每帧经 `overlay_view` 拉取、按键经 `on_overlay_event` 回传，`Esc` 关闭时会回调 `OverlayEvent::Closed`）
- **`Continuation`**：扩展自主续跑：投一条消息并触发下一轮 prompt（忙碌时跳过，避免抢跑用户输入）
- **`AbortRun`**：扩展请求中止当前回合（与 Esc 同款硬中止，但保留用户排队输入）；`loop-detect` 检测到循环时使用
- **`RestartRun { message }`**：扩展请求「中断并重开一轮」——先按 `AbortRun` 中止进行中的回合，再把运行中 steer 注入箱里的排队消息与 `message` 合成一个 user 批次，经 `PromptBatch` 立即作为新一轮 prompt 投出（`hang-detect` 假死恢复时使用；与 `Continuation` 的区别是携带 steer、与 `AbortRun` 的区别是自动重开）。**不动**本地 follow-up 队列：follow-up 是“settle 后才发送”，保持中断前行为，由新一轮结算后自然取走
- **`RewindLastUserInput { restore_to_editor }`**：扩展请求回退最后一次用户输入——TUI 转 `AgentCommand::Rewind`，worker 把活动分支叶子移到最后一条 user 消息之前（该消息与之后的全部输出离开上下文；历史仍保留在会话树）。`restore_to_editor` 决定被回退的输入是否回填输入框；`rewind` 扩展使用
- **`ShowSettings`**：打开指定扩展（`ext`）的设置面板（一次只展示该扩展的一组）
- **`ShowDock`**：请求显示停靠面板（状态栏上方）。幂等入口 [`request_show_dock()`]：队列里已有 `ShowDock` 时不重复入队（一次 fan-out 派发几十个 agent 时，避免把后续完成通知挤到几十轮之后）
- **`RebuildTools`**：扩展工具可见性变化后重建工具列表（下一轮生效）
- **`PersistSessionEntry`**：向当前会话追加自定义条目（扩展持久化，完成后回执 `extension:entry_persisted`）

`plan-mode` 用它弹出「执行计划 / 停留 / 精炼」选择面板并显示完成通知；`loop-detect` 只用一个 `AbortRun` 终止检测到循环的 run；`hang-detect` 用 `RestartRun` 中止假死回合并带上排队消息重开；`goal` 用 `Select` 确认替换目标、`Continuation` 自主续跑、`PersistSessionEntry` 持久化目标状态。



## 相关文档

- [sdk.md](sdk.md)：把 prux 作为 Rust 库嵌入；注册扩展 / 直接调用工具的编程接口
- [settings.md](settings.md)：`disabledExtensions` / `enabledExtensions` / `extensionMode` 配置键
- [http-proxy.md](http-proxy.md)：**出站** HTTP 代理与 `NO_PROXY`（与 `proxy` 扩展方向相反）
- [themes.md](themes.md)：`extra-themes` 同步的主题文件
- [virtual-models.md](virtual-models.md)：虚拟模型（`jet` 扩展即其开箱用例）
- [compaction.md](compaction.md)：压缩钩子（`custom_summarize` 等）
- [keybindings.md](keybindings.md)：扩展快捷键与内置键位
- [tui.md](tui.md)：TUI 交互与面板
