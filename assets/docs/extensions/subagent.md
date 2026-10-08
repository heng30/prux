# 子代理（Sub-agents）

`subagent` 扩展把**子任务委托给独立的子代理**：每个子代理有自己的对话、会话文件、工具集
与模型，跑完后只把结果交回。它是 prux 里唯一能真正并行开工的机制，并且自带一个确定性的
编排器（`SubagentWorkflow`）。

- 扩展名：`subagent`（settings 的 `enabledExtensions` / `/extension` 面板切换）
- 代码位置：`src/extensions/subagent/`
- 来源：移植 `@tintinweb/pi-subagents` v0.19.0（偏离清单见仓库根 `migration-subagents.md`）
- **默认关闭**：`default_enabled() == false`，只在 `Dev` / `Creator` 扩展模式下注册。
  开启方式：`/extension` 面板，或 `settings.json` 的 `"enabledExtensions": ["subagent"]`。

开启后模型可见的工具：`Agent`、`get_subagent_result`、`steer_subagent`，以及（条件性）
`SubagentWorkflow`。用户侧入口是 `/agents` 命令。

> 只想要"某个任务交给一个子代理跑"时，用 `Agent` 就够；要**根据运行时发现**决定派几个、
> 或需要可复现的多阶段流程时，才用 `SubagentWorkflow`（见 [工作流](#工作流subagentworkflow)）。

## 目录

- [心智模型：三条派发路径](#心智模型三条派发路径)
- [子代理运行在什么环境](#子代理运行在什么环境)
- [Agent 类型与 frontmatter 键](#agent-类型与-frontmatter-键)
- [Agent 工具的参数](#agent-工具的参数)
- [工作流（SubagentWorkflow）](#工作流subagentworkflow)
- [设置键（subagent.json）](#设置键subagentjson)
- [功能键之间的关系](#功能键之间的关系)
- [界面：dock、FleetView、检查器、向导](#界面dockfleetview检查器向导)
- [定时任务](#定时任务)
- [事件与 RPC](#事件与-rpc)
- [与其他模块的关系](#与其他模块的关系)

---

## 心智模型：三条派发路径

子代理不是只有一种产生方式，但**无论从哪条路来，最终物化出的都是同一种东西**：

```
Agent 工具（主会话）        ┐
嵌套委派（子代理自己）      ├─→ manager::dispatch → build_spec → 核心 materialize_sub_agent
SubagentWorkflow 脚本      ┘
定时任务（schedule）        ┘
```

`src/core/extensions/tools.rs` 划定了边界：核心只提供**机制**（按 `SubAgentSpec` 物化一个
状态隔离的 child，并交回 `SubAgentRunner` + `SubAgentControls`），spawn 策略、并发、排队、
结果存储、通知全在扩展里。

| 路径 | 入口 | 谁做编排 | 默认前后台 | 生命周期事件 |
|---|---|---|---|---|
| 直接派发 | `Agent` 工具（模型） | 模型的多轮对话 | 后台（`backgroundDefault`） | 发 `subagents:*` |
| 嵌套委派 | 子代理里的 `Agent` 工具 | 另一个子代理 | **前台**（父代理结束会连带掐掉子孙） | 不发（非顶层） |
| 工作流 | 脚本里的 `agent()` | JS 脚本（沙箱确定性执行） | 前台（run 内 await） | 不发（`workflow_owned`） |
| 定时任务 | `Agent({ schedule })` | 墙钟 + 调度表达式 | 后台且 `bypass_queue` | 发 `subagents:scheduled` |
| `@handle` | 输入框 | 用户 | direct 立即派 / model 屏幕外 clone | 发（clone 视作顶层） |

**并行**：`Agent` 工具的 `execution_mode` 是 `Parallel`，所以"一条消息里发多个 `Agent`
调用"就会并发执行（工具描述里明确这么建议）。要更多并发或依赖关系的编排，才需要工作流。

---

## 子代理运行在什么环境

**进程内协程，不是子进程。** 核心 `materialize_sub_agent()` 从父级模板
`Agent::child_from_template(tpl)` 造一个状态隔离的 child `Agent`。

### 状态隔离

| 维度 | 行为 |
|---|---|
| 对话 | 默认全新（prompt 必须自包含）；`inherit_context: true` 复制父级消息；mention clone 用实时压缩感知的 `context_messages` |
| 会话 | 独立 JSONL 子会话，`session_parent_id` 挂父会话（`/resume` 树里可见），可 `resume` |
| 模型 / thinking | 解析成精确 `provider/modelId`，解析失败**直接报错**（不静默继承）；thinking 按目标模型能力钳制 |
| 系统提示 | `prompt_mode: replace`（完整替换 + 清空 `context_files`）或 `append`（追加桥接段）；可再叠加技能预载与记忆块 |
| 工具表 | 父级全集 → frontmatter 白名单收窄 → 剔除子代理工具（递归防护）→ `isolated` 去扩展工具 → `extensions` 作用域 → 注入 `StructuredOutput`（无条件） |
| 事件流 | 每个 child 独立 `json_sink`，扩展据此累计 token / 工具次数 / 转写 |
| 取消 | 每个 child 一个 `abort` 标志；可 `steer`（下一轮 LLM 调用前生效） |

### 工作目录

- 默认**继承父级 cwd**。
- `isolation: "worktree"`（frontmatter 或调用参数）：`git worktree add --detach` 到
  `$TMPDIR/prux-agent-<id>-<pid>`，保留子目录层级（monorepo 包不会悄悄扩成整仓）。
  跑完干净就删；有改动就提交到分支 `prux-agent-<id>`（撞名加时间戳）留在主仓库，结果正文
  附 `[isolation: worktree — changes committed to branch …]`。
  **非 git 仓库 / 无 commit / 建副本失败 → 严格失败**，不静默回退到主目录。
- 副本看不到主 checkout 里未提交/已暂存的改动。

### 落盘产物

统一在任务目录下（`<tmp>/prux-subagents-<uid>/<encoded-cwd>/<session>/tasks/`）：

- `<id>.output`：`.output` 运行转写（每个 child 一份 JSONL，`outputTranscript` 开启时）
- `<runId>.workflow.jsonl`：工作流 journal（前缀重放用）
- 工作流脚本落盘副本（迭代方式是"改文件 + `scriptPath` 重跑"）

子会话本身落在正常会话目录，可以 `/resume`。

### 调度与生命周期

- 状态是**进程级内存态**（不做持久化）：`/new`、`/resume`、`/import`、`/fork` 切换会话
  → 全部中止并清空；扩展被禁用同理。
- 两个并发池刻意分开：后台 `maxConcurrent`（默认 10，超出 FIFO 排队，`queued` 不占额度）；
  前台 `foregroundMaxConcurrent`（默认 0 = 不限）。
- 嵌套深度：主会话 0、子代理 1、孙代理 2；`maxSubagentDepth` 默认 2，`0/1` 即关掉嵌套。
  子代理要拿到委派工具，还必须在自己的 `.md` 里写 `allowed_subagents`。
- 属主作用域：嵌套子代理只能看见**自己派发**的 child；父代理结束时它的子孙一并中止。
- agent id 为 8 位十六进制；另有从类型名派生的 `@handle` 与从 `name` slug 化的 `@alias`。

---

## Agent 类型与 frontmatter 键

类型决定了子代理"是什么"：工具集、模型、提示词、隔离、记忆。

### 发现与优先级

1. 项目级 `<cwd>/.prux/agents/<name>.md`（**受项目信任门控**，未信任则整目录忽略并告警）
2. 全局级 `<agent_dir>/extensions/agent/<name>.md`
3. 内嵌默认三类型

优先级 `项目 > 全局 > 内嵌默认`；同层内后加载者胜并告警。**每次 spawn 现扫目录**，改 `.md`
立刻生效（工具描述里的类型清单随下次 rebuild 更新）。`disableDefaultAgents` 关掉内嵌三类型。

内嵌默认：

| 类型 | 工具 | 提示模式 | 用途 |
|---|---|---|---|
| `general-purpose` | 全部 | `append`（父级孪生） | 通用多步任务 |
| `Explore` | 只读 | `replace` | 代码库探索 |
| `Plan` | 只读 | `replace` | 方案设计 |

### frontmatter 键表

`.md` = YAML frontmatter + 正文（正文即系统提示，在 `replace` 模式下是完整提示）。

| 键 | 取值 | 作用 |
|---|---|---|
| `name` | 字符串 | 派发名（`subagent_type` 匹配键，大小写不敏感；不能含 `:`） |
| `display_name` | 字符串 | UI 标签（cosmetic） |
| `description` | 字符串 | 工具描述里展示的用途 |
| `color` | 颜色名/十六进制 | 列表里的 badge 颜色 |
| `tools` | 列表，或 `*`/`all`/`none` | 工具白名单；支持 `ext:<扩展>` 与 `ext:<扩展>/<工具>` 选择器 |
| `disallowed_tools` | 列表 | 从最终白名单里再剔除这些名字 |
| `allowed_subagents` | `true`/`all`/`*` 或列表 | **开启嵌套委派**并限定可派的类型（缺省 = 不开启） |
| `model` | `provider/id`（fuzzy 可） | 钉死型号；**调用参数覆盖不了** |
| `thinking` | `off`…`max` | 钉死思考档位 |
| `max_turns` | 整数 | 钉死轮数上限 |
| `prompt_mode` | `replace`（默认）/ `append` | 正文是替换还是追加 |
| `enabled` | bool | `false` → 不参与派发：模型可见的类型清单、`Agent` / workflow 解析都会跳过它（`/agents disable` 写这行）；类型定义仍保留在 `/agents` 列表里，可再 `enable` |
| `persist_session` | bool | 是否落盘子会话（覆盖 `rememberAgents`） |
| `session_dir` | 路径 | 子会话目录覆盖 |
| `isolation` | `worktree` | 钉死隔离方式（覆盖调用参数） |
| `isolated` | bool | 只用内置工具，不带任何扩展工具 |
| `extensions` | `false` / 列表 | `false` = 无扩展工具；列表 = 只用这些扩展的工具；缺省 = 全部 |
| `exclude_extensions` | 列表 | 排除这些扩展的工具 |
| `skills` | `false` / 列表 | `false` = 不继承技能索引；列表 = 预载这些技能正文 |
| `memory` | `user`/`project`/`local` | 开启持久记忆（见下） |
| `output_transcript` | bool | 覆盖全局 `.output` 开关 |
| `run_in_background` | bool | 钉死前后台（覆盖调用参数） |
| `inherit_context` | bool | 钉死是否 fork 父级上下文 |

示例：

```markdown
---
name: reviewer
description: Reviews a diff and reports concrete problems
tools: [read, grep, bash]
model: anthropic/claude-haiku-4-5
memory: project
allowed_subagents: [Explore]
---
You are a code reviewer. Read the diff, then report findings with file:line.
```

### 权威性（谁覆盖谁）

合并只发生在一处（`build_spawn_request` / `SpawnRequest::from_type`），规则统一：

| 字段 | 优先级（高 → 低） |
|---|---|
| `model` / `thinking` | frontmatter > 调用参数 > 继承父级 |
| `max_turns` | frontmatter > 调用参数 > `defaultMaxTurns`（0 = 不限） |
| `isolation` / `isolated` | frontmatter > 调用参数（`extensions: false` 等价 `isolated`） |
| `inherit_context` | frontmatter > 调用参数 > `false` |
| `run_in_background` | frontmatter > 调用参数 > 顶层用 `backgroundDefault`，嵌套固定前台 |
| `tools` / `disallowed_tools` / 作用域 | 只由 frontmatter 决定（调用方给不了） |
| `output_transcript` | frontmatter > 全局 `outputTranscript` |
| `persist_session` | frontmatter > `rememberAgents` |
| `name` / `resume` | 调用参数 |

`model` 还会带上"来源"标签：调用方给的越界型号**拒绝**，agent 文件钉的越界型号**只告警**
（`scopeModels` 开启时）。

`/agents` 子命令操作这些文件：`types`、`result`、`stop`、`workflows`、`schedules`、`settings`、
`eject`、`enable`、`disable`、`delete`、`reset`、`edit`。其中改 `enabled` 是**逐行编辑**
（保留注释与键序）；`eject` 把内置类型写成 `.md`；`reset` 删掉覆盖文件恢复内置默认。

---

## Agent 工具的参数

`Agent`（必填：`prompt`、`description`、`subagent_type`；`additionalProperties: false`）：

| 参数 | 说明 |
|---|---|
| `prompt` | 任务正文。**必须自包含**——子代理看不到本会话 |
| `description` | 3~5 词简述（UI 展示；定时任务也拿它当任务名） |
| `subagent_type` | 类型名（未知/禁用按 `fallbackSubagent` 回退或报错） |
| `name` | 记忆名，供 `steer_subagent` / `get_subagent_result` 使用 |
| `model` | 精确或 fuzzy 型号，默认继承父级或类型钉死 |
| `thinking` | 思考档位（按目标模型钳制） |
| `max_turns` | 轮数上限，0/缺省 = 不限 |
| `run_in_background` | 默认由 `backgroundDefault` 决定；`false` 阻塞并内联返回结果 |
| `resume` | 续跑已结束的 agent（id 或 name）；新调用则是全新子代理 |
| `inherit_context` | fork 父级对话 |
| `isolated` | 只用内置工具 |
| `schedule` | 注册为定时任务而不是立刻跑（`schedule: on` 时才出现在 schema 里） |
| `isolation` | `off` / `worktree`（`worktreeIsolation: on` 时才出现） |

配套两个工具：

- `get_subagent_result({ agent_id, wait?, verbose? })`：查状态、读结果。`wait: true` 阻塞等待；
  `verbose: true` 附转写（截断到最后 200 行 / 8 KB）。读过结果的 agent 不再发完成通知。
- `steer_subagent({ agent_id, message })`：给运行中的 agent 注入消息（当前工具调用之后生效）。

后台调用的返回文本是"已启动，完成时通知你，别轮询"；排队时是"已排队，额度空出来自动启动"。

---

## 工作流（SubagentWorkflow）

`SubagentWorkflow` 不是"另一种子代理"，而是**一个跑在沙箱里的 JS 剧本**——脚本自己没有模型
和上下文，它的作用是在脚本里调 `agent()` 派发子代理。编排决策因此从"模型的多轮对话"搬进
"一段可复现的确定性代码"。

**工具立刻返回 task id**，运行在后台继续，完成时经 join 批次发一条 `Continuation` 通知模型
（不要轮询/睡觉等）。

### 脚本来源与 `meta`

优先级 `scriptPath` > `script` > `name`。

- 具名工作流：`<cwd>/.prux/workflows/<name>.js`（项目级，受信任门控）→
  `<agent_dir>/extensions/workflows/<name>.js`（全局），first-hit-wins。
  目录里只有带 `export const meta =` 声明的 `.js` 才算工作流（纯正则判断，从不执行）。
- `meta` 是**字面量提取**（不执行 JS），字段：`name`、`description`、`whenToUse?`、
  `phases?[{title, detail?, model?}]`。`meta` 写错属于工具错误，在登记运行**之前**就报错。

### 沙箱与全局函数

内核是 `rquickjs` 的 `AsyncRuntime`/`AsyncContext`，注入：

| 全局 | 作用 |
|---|---|
| `agent(prompt, opts?)` | 派一个子代理；无 `schema` 返回最终文本，有则返回校验过的对象 |
| `parallel(thunks)` | 屏障式并发；抛错的 thunk 变成 `null` |
| `pipeline(items)` | 流式并发（不等最慢的）——工具描述建议优先用它 |
| `phase(title)` | 定义/切换阶段（进度分组） |
| `log(msg)` | 写进度日志 |
| `workflow(name, args)` | 内联跑另一个具名工作流（**只允许一层嵌套**，上限 256 次） |
| `args` | 工具传入的任意 JSON |
| `budget` | `spent()` / `remaining()`（按 output token 计） |

为了 journal 重放不漂移，脚本里 `Date.now()`、无参 `new Date()`、`Math.random()`、`eval()`、
`new Function()` 一律抛错。

`agent()` 的 `opts`：`label`（进度与 `resume` 的定位键）、`phase`、`model`、`agentType`、
`effort`、`isolation`、`schema`、`gate`、`resume`。其中 `resume` 与
`agentType`/`model`/`effort`/`isolation`/`gate`/`schema` 互斥（续跑沿用启动时的设定）。

- `schema`：给 child 注入 `StructuredOutput` 工具，返回校验后的对象；不符会被驳回让 child
  重答，始终没答出来则该次调用返回 `null`。
- `gate`：child 成功后跑一条 shell 命令，非零退出或超时（10 分钟）即算该 child 失败。

### 与子代理的关系

脚本里的 `agent()` 最终走 `manager::dispatch`，因此物化环境与手动 `Agent` **完全一致**
（独立会话、工具白名单、worktree 隔离、嵌套限深……）。差别只有三点：

1. `run_in_background = false`（对这次 run 是前台 await），并发由 bridge 的 permit 控制
   （`min(16, cpus-2)` 且至少 1）。
2. `workflow_owned = true`：不发 `subagents:*` 生命周期事件、RPC 不可停。
3. 取消信号是**本次尝试**的 abort，而不是启动回合的 `parent_abort`——所以用户在主会话按 Esc
   不会把后台 fan-out 全掐死，而检查器里的 skip/retry/kill 能真正停掉那个 child。

### 配额与边界

单次运行：并发 `min(16, cpus-2)`（至少 1）、累计子代理 1000、单次 `parallel`/`pipeline` items 4096、
嵌套 `workflow()` 256 次、兜底墙钟 30 分钟、跨边界序列化 512 KiB。

### journal 与 `resumeFromRunId`

每条记录按"运行中的位置 + 决定这个 agent 做什么的一切"的哈希作键；重放只复用**前缀**，
第一条对不上就停止复用。失败的记录永不当作成功重放。用了 `agent({ resume })` 的运行整盘不重放。
`resumeFromRunId` 若没另给来源，会自动复用那次运行落盘的脚本文件。

---

## 设置键（subagent.json）

文件：`agent_dir()/extensions/subagent.json`（首次加载自动创建并回填缺省）。键名 camelCase，
与面板同名。面板入口：`/agents settings` 或 `/extension` 面板；改动立即落盘，进程内缓存
按路径+mtime 失效。

| 键 | 默认 | 作用与关联 |
|---|---|---|
| `widget` | `all` | dock 列出哪些子代理：`all` / `background` / `off` |
| `backgroundDefault` | `on` | `Agent` 不写 `run_in_background` 时的默认值（嵌套固定前台） |
| `maxConcurrent` | `10` | 后台并发上限，超出 FIFO 排队 |
| `foregroundMaxConcurrent` | `0` | 前台并发上限，`0` = 不限（工作流的 `agent()` 走这条） |
| `join` | `smart` | 完成通知合批：`smart` 30s / `group` 15s / `async` 逐个 |
| `toolDescription` | `full` | `Agent` 工具描述：`full` / `compact` / `custom`（读 `agent-tool-description.md`） |
| `fallbackSubagent` | `general-purpose` | 未知/禁用类型的处理：回退或 `none` 报错 |
| `strictAgentFiles` | `off` | 坏 `.md` 是致命错误还是跳过 + 告警 |
| `disableDefaultAgents` | `off` | 不注册内嵌三类型 |
| `outputTranscript` | `off` | 是否写 `.output` JSONL 转写 |
| `showTokens` | `on` | dock / 结果行显示 token |
| `showCost` | `off` | 显示 provider 估算成本 |
| `showModel` | `off` | 显示子代理实际生效的型号 |
| `reportUsage` | `off` | 把子代理开销计入主会话统计（挂到下一个真实工具结果上） |
| `agentMentions` | `model` | `@handle` 模式：`off` / `direct` / `model`（clone 对话在屏幕外写 prompt） |
| `maxSubagentDepth` | `2` | 嵌套上限（0/1 = 关掉；还需 `allowed_subagents`） |
| `worktreeIsolation` | `on` | 关掉即"任何路径都不建副本"，并从 `Agent` schema 里去掉 `isolation` |
| `schedule` | `on` | 允许定时任务（`Agent.schedule` 参数 + `/agents schedules`） |
| `scopeModels` | `off` | 把子代理型号限制在 `enabledModels` 内（调用方越界拒绝，frontmatter 越界告警） |
| `workflows` | `auto` | `SubagentWorkflow` 可用性：`auto` / `on` / `off`（见下） |
| `fleetView` | `on` | `/agents` 菜单是否提供 Fleet view 入口 |
| `rememberAgents` | `on` | 默认落盘子会话（frontmatter `persist_session` 覆盖） |
| `defaultMaxTurns` | `0` | 未显式给 `max_turns` 时的默认上限（`0` = 不限） |
| `graceTurns` | `5` | 到轮数上限后的收尾宽限轮数 |

---

## 功能键之间的关系

很多键不是独立开关，而是成对/成组生效的。下面这些关系最容易踩错：

**发现类**

- `disableDefaultAgents` + `strictAgentFiles` + `fallbackSubagent` 一起决定"类型表长什么样、
  坏文件怎么处理、找不到类型时会不会报错"。
- 项目级 `<cwd>/.prux/agents/` 整体受**项目信任**门控：未信任时它不参与发现（不是静默忽略，
  会给一次告警）。

**工具面类**

- `tools`（frontmatter）→ `disallowed_tools` → 递归防护 → `isolated` / `extensions` /
  `exclude_extensions` 作用域：**按这个顺序**依次收窄。`injected_tools`（`StructuredOutput`）
  在最后无条件注入，任何开关都摘不掉。
- `isolated: true` 与 `extensions: false` 等价；`isolated` 只影响**扩展工具**，内置工具照旧。
- `allowed_subagents` + `maxSubagentDepth` 必须同时满足，子代理才真能派出孙代理：
  frontmatter 开启嵌套，深度上限决定能否放行。

**执行方式类**

- `backgroundDefault` ↔ `Agent.run_in_background` ↔ frontmatter `run_in_background`：
  三级优先级，嵌套路径不受前两者影响（固定前台）。
- `maxConcurrent` 管后台池，`foregroundMaxConcurrent` 管前台池；工作流的 `agent()` 走前台池，
  但还有自己的 permit 上限。
- `max_turns`（三级来源）↔ `defaultMaxTurns` ↔ `graceTurns`：前者定上限，后者定"到限后
  还能收尾几轮"。

**隔离类**

- `worktreeIsolation`（总开关）→ 关掉时 `Agent` schema 里**不出现** `isolation`，
  且 frontmatter 的 `isolation: worktree` 被丢弃（不是报错）。
- frontmatter `isolation` 优先于调用参数。

**会话与记忆类**

- `rememberAgents` ↔ frontmatter `persist_session`：决定子会话是否落盘；不落盘就没有
  `resume` 与 `@handle` 续跑可言。
- `inherit_context`（三级来源）与 `context_messages`（mention clone 专用，优先级更高）都改
  child 的初始对话。
- 技能：`skills: false` 清空继承的技能索引；列表则预载正文进提示。
- 记忆：`memory: user`（`<agent_dir>/agent-memory/<name>/`）、`project`
  （`<cwd>/.prux/agent-memory/<name>/`）、`local`（`…/agent-memory-local/<name>/`）。
  project/local 读写都要求项目受信任；开启后会自动给 child 补记忆读写工具 + 注入 `MEMORY.md`
  前 200 行。

**模型类**

- `scopeModels` ↔ `model`（调用参数 / frontmatter）：开启后，调用方的越界型号被拒绝，
  agent 文件钉的越界型号只告警。
- 型号解析在扩展侧做 fuzzy → canonical（核心只接受精确 `provider/id`），解析失败直接报错。

**显示类**

- `widget` / `showTokens` / `showCost` / `showModel` 只影响展示，
  不影响执行。
- `reportUsage` 会把子代理开销挂到主会话的下一个工具结果上（尽力而为）。

**工作流类**

- `workflows` 决定 `SubagentWorkflow` 是否进入工具表，也决定 `/agents` 菜单是否出现
  `workflows` 入口。`auto` 时的判定规则见 [与其他模块的关系](#与其他模块的关系)。

---

## 界面：dock、FleetView、检查器、向导

- **dock**（`widget` 控制）：agent 段最多 6 行（已终结最多 3 行），工作流段最多 3 个运行。
- **FleetView**（`/agents view`，`fleetView` 开关）：以**选择器面板样式**渲染（与 `/agents list`、
  `/agents workflows`、`/agents schedules` 同一布局：占据输入框区域、隐藏主页输入框，
  结构为 `─` / 空行 / 列表 / 空行 / 键位提示 / 空行 / `─`）；↑↓ 选择、Enter 打开查看器、
  `s` 停止、`r` 续跑、Esc 关闭。agent 行与工作流运行行合并展示。
- **工作流列表**（`/agents workflows`）：↑↓ 选择、Enter 进工作流检查器、Esc 关闭；
  运行中的运行额外提示 `/agents stop <id>`。
- **会话查看器**（全屏）：转写用**与主页消息区同一条**渲染管线渲染（user 背景块、
  assistant Markdown、tool 调用/输出块、thinking、表格、代码高亮、latex、mermaid 一致），
  状态头/路径/活动行/操作回显固定在顶部不随滚动；`i` 聚焦 steer/续跑输入行、Enter 提交、
  `s` 停止、`r` 续跑、`Ctrl+O` 展开/折叠工具输出、`Ctrl+T` thinking 显隐、
  ↑↓/PgUp/PgDn/Home/End 与鼠标滚轮/右侧滚动条滚动（在底部才自动跟随），
  拖选内容 + `Ctrl+Shift+C`/中键复制。
- **工作流检查器**（两栏）：`j`/`k` 或 ↑↓ 移动、`f` 切换扁平/分组、`p` 暂停/恢复运行
  （在跑的跑完，不再启动新的）、`s` 跳过选中的 agent、`r` 重试、`x` 停整个运行、
  `c`/Enter 打开选中 agent 的会话查看器。
- **定时任务列表**（`/agents schedules`）：同样的选择器面板样式；↑↓ 选择、`d` 取消、Esc 关闭。
- **创建向导**（`/agents create`）：分步表单（位置 → 名称 → 描述 → 工具 → 型号 → 思考 →
  系统提示 → 汇总）。汇总步 Enter 直接写 `.md`，`g` 把生成提示交给主模型由 `write` 落地。
- **类型编辑**（`/agents edit <type>`）：核心多行编辑器，改整个 `.md` 内容。

完成通知的两种形态：给模型的 `Continuation`（完整结果，落库）与给人的卡片
（`subagent-completion`，只留预览）。

---

## 定时任务

开关 `schedule`。两种创建方式：模型用 `Agent({ schedule })`（此时该次 spawn **不立刻跑**，
而是登记为任务），或用户经 `/agents schedules` 查看/取消。

调度表达式四种写法（判定顺序即此顺序）：

| 写法 | 类型 | 例 |
|---|---|---|
| `+10m` / `+1h` / `+2d` / `+30s` | 一次性（相对） | 10 分钟后跑一次 |
| `5m` / `1h` / `2d` / `30s` | 周期 | 每 5 分钟 |
| ISO 时间戳 | 一次性（绝对） | `2026-02-14T09:00:00` |
| 6 段 cron | cron（本地时间） | `0 0 9 * * 1` = 每周一 9:00 |

存储：`<agent_dir>/extensions/schedules/<session>.json`（按会话键，原子写）。到点走的是**同一条**
`manager::dispatch`，但带 `bypass_queue`：不占后台额度也不排队（配额是给人的突发行为用的）。
过期的一次性任务会被标记错误并停用，同时告警（不静默丢）。

---

## 事件与 RPC

顶层（非嵌套、非工作流）agent 的生命周期事件发到核心事件总线：

`subagents:ready`、`subagents:created`、`subagents:started`、`subagents:steered`、
`subagents:compacted`、`subagents:completed` / `subagents:failed`、`subagents:scheduled`、
`subagents:scheduler_ready`。

RPC（协议版本 2）：`subagents:rpc:ping`、`rpc:spawn`、`rpc:stop`、`rpc:consume`，
回执发到 `<channel>:reply:<requestId>`。只有顶层记录可被 RPC 停。

---

## 与其他模块的关系

**让位判定（`workflows`）**：`off` 从不提供；`on` 永远提供；`auto`（默认）在发现**别的扩展**
提供了 `Workflow` / `workflow` / `SubagentWorkflow` 这几个工具名（精确匹配）时，本扩展的
工作流**整项让位**，并给一次说明。改名或调 `on`/`off` 需要重算判定（设置变更会自动失效缓存）。

**核心接缝**：扩展只依赖 `core::extensions` 里的 `SubAgentSpec` / `SubAgentRunner` /
`SubAgentControls` / `ToolExecCtx`（`cwd`、`make_sub_agent`、`parent_abort`、
`agent_id`、`depth`、`parent_model`），不碰核心内部。

**子代理工具默认被剔除**：核心从 child 的工具表里强制移除 `Agent` / `get_subagent_result` /
`steer_subagent`（递归防护），只有扩展按 `allowed_subagents` + 深度上限显式放行的那一层例外。

**项目信任**：项目级 agent 定义、项目级具名工作流、project/local 记忆都走同一道信任门控。

**CLI flag**：`--subagents-workflow-file <path>` 在会话拿到可物化的执行上下文后跑一个
工作流文件（与工具调用同一条路径）。
