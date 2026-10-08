# Prux 迁移指南：pi-tasks → prux 扩展 `tasks`

> **来源**：`@tintinweb/pi-tasks` v0.9.0（`pi-tasks/` → `tmp/pi-tasks`，独立 git 检出，HEAD `29180d7`）
> **目标**：`src/extensions/tasks.rs`（门面）+ `src/extensions/tasks/`（模块）
> **性质**：本文档是先写后做的**契约冻结版**——第 0–5 章在动代码前定稿，第 2/6/7/8 章的
> 状态列在实施过程中就地回填。与 `migration-v0.87.1.md`（pi 0.85→0.87 版本升级）不同，
> 本文档记录的是**一个上游插件整体移植进 prux 扩展体系**的映射、偏离与缺口。
>
> **与 `migration-subagents.md` 的关系**：pi-tasks 与 pi-subagents **同一作者**
> （`@tintinweb`），二者通过 pi 的 eventbus 做 scoped request/reply RPC 联动。
> subagent 扩展已迁入 prux 并已实现**协议 v2** 的全部 RPC 端点（见 §2.7），
> 因此本扩展的联动面在语言层面**已经就绪**，移植时主要工作是复用而非新建。
>
> **更新记录**：
> - `设计与分析定稿`：第 0–8 章首版。上游 11 个文件、2 986 行 `src/`、6 181 行测试逐文件对照；
>   核心结论是**预期零核心改动**（复用 subagent 迁移时已建的 `ContextWithSystem` 与
>   扩展事件总线），全部落点集中在扩展层。
> - `Tier1–4 实施完成`：`src/extensions/tasks.rs` + `src/extensions/tasks/{types,store,paths,
>   config,glyphs,sort,auto_clear,cadence,widget,menu,tools,subagent}.rs` 全部落地；
>   `extensions.rs` 加 `pub mod tasks;` + `PRIORITY_TASKS = 78`。**核心改动为零**，与预测一致。
>   54 个扩展单测通过。实施中的 3 处实际修正与 2 处未做项见 §9。
> - `缺口补齐`：两项未做项落地——§3.2 `/tasks create` 带参子命令（不引入 overlay）、
>   §3.13 fork seed（读会话文件头 `parent_session_id`）；§3.15 提醒注入与 §3.1/#9 dock 堆叠
>   顺序补上回归测试。仅剩 `PRUX_TASKS_DEBUG` 未接（§9.4）。

---

## 〇、摘要与 Tier 路线图

### 上游规模

| 维度 | 数值 |
|---|---|
| TS 源码 | 2 986 行 / 11 个文件（`src/` 9 + `src/ui/` 2） |
| 最大单文件 | `src/index.ts` 1 351 行（7 工具 + `/tasks` 菜单 + hook 接线 + subagent RPC） |
| 测试 | 21 个 `*.test.ts` / 6 181 行（含 `test/helpers/mock-pi.ts` 假宿主） |
| 文档 | `README.md`（23 KB）、`CUSTOMIZING.md`（14 KB）、`CHANGELOG.md`（38 KB）、`AGENTS.md` |
| 依赖 | `typebox`；peer `@earendil-works/pi-coding-agent` + `@earendil-works/pi-tui` |

### 迁移原则

1. **契约对齐优先于实现对齐**：7 个工具名（`TaskCreate`/`TaskList`/`TaskGet`/`TaskUpdate`/
   `TaskOutput`/`TaskStop`/`TaskExecute`）、参数名、状态词（`pending`/`in_progress`/`completed`/
   `deleted`）、依赖字段（`blocks`/`blockedBy`）、工具输出文案照抄上游——它们刻意镜像
   Claude Code 的调用约定，是模型习惯的一部分。内部实现按 prux 风格重写，不做逐行翻译。
2. **策略全在扩展，机制复用核心**：本扩展是**纯策略扩展**。上游需要的三类机制——
   子代理物化、跨扩展事件总线、请求级上下文注入——prux 在迁移 subagent 时已经建好
   （`SubAgentSpec` / `core::extensions::events` / `ContextWithSystem`）。预期**零核心改动**（§5）。
3. **上游死代码不迁移**：`ProcessTracker` 在 `index.ts` 中**没有任何生产者**（`track()` 仅被
   测试调用），对应上游 README 的 Future Work "Background Bash auto-task creation"；
   prux 的 bash 工具同样没有 `run_in_background`。见 §2.8 / §3.11。
4. **有意偏离必须留痕**：每处因宿主机制差异产生的偏离（widget 载体、输入原语、设置面板、
   瞬态注入钩子、会话钩子形状、项目信任门控…）在 §3 记录「上游行为 / prux 行为 / 理由 /
   用户动作 / 收敛时机」。

### Tier 路线图

| Tier | 主题 | 内容 | 状态 |
|---|---|---|---|
| **Tier1** | 任务内核 | 任务模型 + 文件/内存存储（含原子写、信封校验、规范化）+ 4 个核心工具（`TaskCreate` / `TaskList` / `TaskGet` / `TaskUpdate`）+ 双向依赖与警告 + `/tasks` 基础菜单（查看/清理） | ✅ 已实施 |
| **Tier2** | Widget 与设置 | dock 任务列表（glyphs / 排序 / 折叠 / 溢出 / 截断）+ spinner + elapsed/token 指标 + 默认启用/禁用扩展设置 + `/tasks settings`（原生设置面板）+ 会话生命周期（新建/恢复/切换/叉开）+ auto-clear + `/tasks create` 带参子命令 | ✅ 已实施 |
| **Tier3** | subagent 联动 | presence 握手（ping/ready）+ `TaskOutput` / `TaskStop` / `TaskExecute` + `agentTaskMap` + reload 重连（reattach）+ auto-cascade + 依赖结果注入 + 完成/失败事件处理 | ✅ 已实施 |
| **Tier4** | 环境与收尾 | 四种存储作用域（`memory` / `session` / `session-global` / `project`）+ 环境变量覆盖（`PRUX_TASKS`）+ 系统提醒 cadence 注入 + 跨进程文件锁 + fork seed + `PRUX_TASKS_DEBUG` | ✅ 已实施（`PRUX_TASKS_DEBUG` 未接，见 §9.4） |

**各 Tier 的验收边界**：Tier1 结束时 7 个工具中 4 个可用且行为与上游一致；Tier3 结束时
`TaskExecute` → `TaskOutput` → auto-cascade 全链路在 prux 的 subagent 扩展上跑通；
Tier4 结束时四种作用域与两个环境变量语义与上游对齐。

### 状态取值

| 标记 | 含义 |
|---|---|
| ✅ | 已实现 |
| 🔶 | 待实现 |
| ➖ | 决策不迁移（附理由） |
| ⚠️ | 已实现但有偏离（见 §3） |
| 🟢 | 偏离已收敛 |

---

## 一、架构对照

### 1.1 上游模块 → prux 落点

上游 11 个文件按职责分四组。下表是全部文件的归宿。

#### A. 任务模型与存储

| 上游文件 | 行数 | 职责 | prux 落点 |
|---|---|---|---|
| `types.ts` | 40 | `TaskStatus` / `Task` / `TaskStoreData` / `BackgroundProcess` | `tasks/types.rs` |
| `task-store.ts` | 390 | 内存/文件存储、CRUD、双向依赖、`normalizeTask`、信封校验、原子写、`snapshot`/`seed` | `tasks/store.rs` |
| `task-paths.ts` | 65 | 四种作用域的落点、`projectKey` 编码、全局会话目录回收 | `tasks/paths.rs` |

#### B. 展示与配置（纯数据 + 纯函数）

| 上游文件 | 行数 | 职责 | prux 落点 |
|---|---|---|---|
| `task-glyphs.ts` | 110 | glyph 默认值、控制字符/双向覆写校验、逐项回退 | `tasks/glyphs.rs` |
| `task-sort.ts` | 90 | 排序预设（`id`/`status`/`active`/`recent`/`oldest`）与排序 spec、健壮解析 | `tasks/sort.rs` |
| `tasks-config.ts` | 77 | 全局默认 + 项目覆盖逐键合并（`glyphs` 深一层）、只写差异项 | `tasks/config.rs` |
| `reminder-cadence.ts` | 90 | 提示注入的纯 cadence 状态机（可单测） | `tasks/cadence.rs` |
| `auto-clear.ts` | 134 | 回合制自动清理（两种模式 + 批次边界） | `tasks/auto_clear.rs` |
| `ui/settings-menu.ts` | 174 | `ui.custom` + pi-tui `SettingsList` 设置面板 | ➖ 不迁移 → 改用 prux **原生扩展设置面板**（`settings()` / `apply_setting`），见 §3.3 |

#### C. UI

| 上游文件 | 行数 | 职责 | prux 落点 |
|---|---|---|---|
| `ui/task-widget.ts` | 325 | 常驻任务列表（glyph/spinner/elapsed/token/溢出/折叠/截断） | `tasks/widget.rs`（`dock_lines` 供数）+ 帧同步在门面 |

#### D. 入口与调度

| 上游文件 | 行数 | 职责 | prux 落点 |
|---|---|---|---|
| `index.ts` | 1 351 | 7 工具 + `/tasks` 菜单 + 全部 hook 接线 + subagent RPC 桥 + auto-cascade + reattach | `tasks.rs`（门面：trait 实现 + 工具 schema + 命令接线）+ `tasks/tools.rs`（工具执行）+ `tasks/menu.rs`（`/tasks` 状态机）+ `tasks/subagent.rs`（RPC 桥 + 生命周期监听） |
| `process-tracker.ts` | 140 | 后台进程跟踪（输出缓冲、阻塞等待、优雅停止） | ➖ 不迁移（无生产者，见 §2.8 / §3.11） |

### 1.2 prux 侧文件布局

```
src/extensions/tasks.rs               门面：linkme 工厂 + Extension impl
                                      （tools / commands / hooks / settings / dock_lines
                                        / on_agent_event / on_boundary / transform_context_with_system
                                        / on_session_start / on_session_switched / on_extension_event
                                        / execute_tool_async / on_ui_choice / on_registered）
src/extensions/tasks/types.rs         Task / TaskStatus / TaskStoreData / SortKey / Glyphs
src/extensions/tasks/store.rs         内存 + 文件存储、CRUD、依赖边、原子写、锁、snapshot/seed
src/extensions/tasks/paths.rs         四种作用域落点、projectKey、全局会话目录回收
src/extensions/tasks/config.rs        全局/项目两层配置、合并、差异写回、面板键
src/extensions/tasks/glyphs.rs        glyph 解析与校验
src/extensions/tasks/sort.rs          排序预设与 spec
src/extensions/tasks/cadence.rs       提示 cadence 纯状态机
src/extensions/tasks/auto_clear.rs    回合制自动清理
src/extensions/tasks/widget.rs        任务行构建（供 dock_lines）
src/extensions/tasks/menu.rs          /tasks 菜单状态机（Select + on_ui_choice + overlay 输入）
src/extensions/tasks/subagent.rs      subagent RPC 桥 + 完成/失败监听 + auto-cascade + reattach
src/extensions.rs                     增加 `pub mod tasks;` + `PRIORITY_TASKS`
```

### 1.3 为什么几乎不需要新核心接缝

上游需要宿主提供三类能力，prux 均已在既有接缝上覆盖：

| 上游需求 | 上游做法 | prux 既有接缝 | 证据 |
|---|---|---|---|
| 派发子代理并拿回 id | 经 eventbus 向 pi-subagents 发 `subagents:rpc:spawn` | `core::extensions::events` 哑总线 + subagent 扩展已实现 `rpc_spawn` | `src/extensions/subagent/events.rs` |
| 把提醒**只加进本次请求**、不落盘 | `pi.on("context")` 返回改写后的 messages | `ExtensionHook::ContextWithSystem` + `transform_context_with_system`（"只影响本次请求，不改写会话历史"） | `src/core/agent_loop.rs::apply_context_with_system` |
| 常驻任务列表 | `ui.setWidget(..., {placement:"aboveEditor"})` | `dock_lines` + `request_show_dock` + `wants_redraw` | `src/core/extensions/dock.rs` |

因此本迁移**不新增核心接缝**，唯一的判断点是"用哪个既有钩子"（§3 逐条给出）。

---

## 二、逐特性总表

### 2.1 工具

| 上游工具 | 参数 | prux 落点 | 备注 |
|---|---|---|---|
| `TaskCreate` | `subject`* / `description`* / `activeForm?` / `agentType?` / `metadata?` | `ExtensionTool` + `execute_tool_async` | `agentType` 并入 `metadata.agentType`；`promptGuidelines` 有 3 条 |
| `TaskList` | 无 | 同上 | 输出排序：pending → in_progress → completed（各组按 id），仅显示**未完成**的 `blockedBy` |
| `TaskGet` | `taskId`* | 同上 | 展示 owner、全部边；非空 metadata 以 JSON 展示 |
| `TaskUpdate` | `taskId`* + `status?` / `subject?` / `description?` / `activeForm?` / `owner?` / `metadata?` / `addBlocks?` / `addBlockedBy?` | 同上 | `metadata` 浅合并（`null` 删键）；`status:"deleted"` 永久删除并清理边；返回 `changedFields` + `warnings` |
| `TaskOutput` | `task_id`* / `block?`(默认 true) / `timeout?`(默认 30000, 上限 600000) | 同上 | 支持任务 ID 与 agent ID（含前缀）；终结后返回结果并**消费**通知（`subagents:rpc:consume`） |
| `TaskStop` | `task_id?` / `shell_id?`(废弃) | 同上 | 子代理路径发 `subagents:rpc:stop`；上游对"有意停止"标 completed |
| `TaskExecute` | `task_ids`* / `additional_context?` / `model?` / `max_turns?` | 同上 + `tasks/subagent.rs` | 要求 `pending` + 有 `agentType` + 依赖全 completed；经 `subagents:rpc:spawn` 后台派发 |

**工具结果形状**：上游 `{ content: [{ type: "text", text }], details: undefined }` → prux
`ToolResult::text(msg)`（`src/core/tools/index.rs`）。

**工具顺序与并发**：上游 `TaskCreate` 的 description 明确引导"一轮内多次调用以并行建批"。
prux 侧 `ExtensionTool.execution_mode` 可声明 `ToolExecutionMode::Parallel`（subagent 的
`Agent` 工具即如此）；7 个任务工具默认可并行，仅 `TaskUpdate`/`TaskOutput` 若涉及等待需注意
`TaskOutput` 的阻塞语义。

### 2.2 命令

| 上游 | 实现 | prux 落点 |
|---|---|---|
| `/tasks` | 递归 `ui.select` 多级菜单：View all / Clear completed / Clear all / Settings（建任务改走 `create` 子命令） | `ExtensionCommand { name: "tasks", busy_safe: true }` + `register_slash_command(EXT, "tasks", handler)` + `tasks/menu.rs` 状态机（见 §3.13） |
| 子命令 | 无（全靠递归菜单） | prux 侧可补 `subcommands`（`settings` / `clear` / `create` / `view`）以获得候选补全，但需与 handler 解析保持一致 |

`busy_safe: true` 的理由与 subagent 一致：handler 只读/改扩展自身状态、发 UI 请求，
**不锁 agent**。

### 2.3 存储与作用域

| 上游 `taskScope` | 上游落点 | prux 落点 |
|---|---|---|
| `memory` | 无文件 | 无文件 |
| `session`（默认） | `<workspace>/.pi/tasks/tasks-<sessionId>.json` | `<cwd>/.prux/tasks/tasks-<sessionId>.json` |
| `session-global` | `<agent-dir>/tasks/sessions/<project-key>/tasks-<sessionId>.json` | `<agent_dir()>/extensions/tasks/sessions/<project-key>/tasks-<sessionId>.json` |
| `project` | `<workspace>/.pi/tasks/tasks.json` | `<cwd>/.prux/tasks/tasks.json` |

- `<agent-dir>`：上游 `getAgentDir()`（默认 `~/.pi/agent`）→ prux `settings_manager::agent_dir()`
  （默认 `~/.prux`，测试 override / `$PRUX_AGENT_DIR` 优先）。
- `<project-key>`：上游 `--<path with / and : → ->--`；prux 会话日志采用同一编码，直接复用上游
  逻辑即可（§3.6）。
- `session-global` 的"只决定新文件落点、workspace 已有文件仍优先"语义需原样保留（`task-paths.ts`
  的 `sessionTaskFile`）。
- 会话文件键：`pi --no-session` 时上游不落盘（`ctx.sessionManager.getSessionFile()` 为空）；
  prux 对应 `Session::get_session_file()` 为 `None`（见 `on_session_start` 的 `session: Option<&Session>`）。
- 空会话文件删除与 `session-global` 目录回收：`store.delete_file_if_empty()` +
  `reclaimGlobalSessionTasksDir()`。
- **跨进程文件锁**：`project` 作用域与 `PI_TASKS`/`PRUX_TASKS` 显式路径允许多会话共享一个
  文件。上游用 `<file>.lock`（`<pid>:<uuid>`，PID staleness + 二次轮询，释放前校验 token）。
  prux 依赖里目前**没有** `fs2`/`fs4`；可用 `nix`（unix 依赖已含 `fs` feature）的
  `fcntl`/`flock`，或原样移植上游 lock-file 协议（见 §3.8）。

### 2.4 Widget 与 UI

| 上游能力 | 上游 API | prux 落点 |
|---|---|---|
| 常驻列表 | `ui.setWidget("tasks", cb, { placement: "aboveEditor" })` | `ExtensionHook::Dock` + `dock_lines()`（`tasks/widget.rs` 构建行） |
| 有任务时自动显示 | `setWidget` 即注册 | `request_show_dock()`（幂等） |
| spinner 动画 | `setInterval(150ms)` 自增帧并 `update()` | `wants_redraw() == true` 时 TUI 每帧重绘；帧号在 `dock_lines()` 内按 **时间** 推进（`now_ms()/150 % frames`），无定时器（见 §3.1） |
| 空列表隐藏 | `setWidget(key, undefined)` / 不返回行 | `dock_lines()` 返回空即自动隐藏 |
| 头部汇总行 `● N tasks (…)` | 自绘 | 同上（`DockSpan`） |
| 状态 glyph / spinner / 溢出 / 折叠 / 截断 | pi-tui `truncateToWidth` | `unicode-width` 或依赖 TUI 逐行截断；`glyphs.truncation` 保留 |
| 输入（建任务） | `ui.input(...)` | ⚠️ prux 无 input prompt 原语 → overlay `OverlayInput` / `OverlayEditor`（见 §3.2） |
| 选择 / 详情 | `ui.select(title, choices)` | `ExtensionUiRequest::Select` + `on_ui_choice(id)` |
| 通知 | `ui.notify(text, level)` | `extensions::request_ui(NotifyRich)` / `crate::extensions::util::notify_text` |
| 设置面板 | `ui.custom` + `SettingsList` | 原生 `settings()` / `apply_setting()` + `/tasks settings` → `ShowSettings{ext:"tasks"}`（见 §3.3） |

### 2.5 系统提醒与 cadence

上游在 `tool_result` 里**只**推进 cadence（刻意不改任何工具输出），真正的注入发生在 `context`
钩子。prux 映射：

| 上游 hook | prux 接缝 | 说明 |
|---|---|---|
| `turn_start` | `on_agent_event` `type == "turn_start"` | `onTurnStart(cadence)` |
| `tool_result` | `after_tool_call_ext`（`AfterToolCall` hook） | 仅用工具名；任务工具重置 cadence |
| `turn_end` | `on_boundary` `type == "turn_end"` | 读 `event.message.usage` 做 token 统计；检测 in_progress 滞留 |
| `agent_settled` | `on_boundary` `type == "agent_before_settle"` | `autoClear.onRunEnded()`；需按 `event.agentScope == "main"` 过滤（见 §3.5） |
| `context` | `transform_context_with_system`（`ContextWithSystem` hook） | **瞬态** 注入，不落盘（见 §3.4） |
| `session_start(reason)` | `on_session_start` / `on_session_switched` | 无 reason 枚举（见 §3.5） |
| `before_agent_start` | — | 无直接对应；惰性 UI 初始化由 `on_exec_ctx` / `on_session_start` 覆盖 |
| `tool_execution_start` | `on_agent_event` `type == "tool_execution_start"` / `on_exec_ctx` | 刷新 `latest_ctx` |

**提醒文案**：`buildSystemReminder` 的空列表提醒 + JSON 状态回显（上限 10 条、超限优先进
in_progress）、`sanitizeField`（折行 + 剥离 `<system-reminder>` 标签）全部可原样移植。

### 2.6 配置与设置

上游两层 JSON（`<agent-dir>/tasks-config.json` 全局默认 + `<workspace>/.pi/tasks-config.json`
项目覆盖，逐键合并，`glyphs` 深一层）：

| 设置键 | 取值 | 默认 | prux 设置面板 |
|---|---|---|---|
| `taskScope` | `memory`/`session`/`session-global`/`project` | `session` | ✅ cycle |
| `autoCascade` | on/off | off | ✅ cycle |
| `autoClearCompleted` | `never`/`on_list_complete`/`on_task_complete` | `on_list_complete` | ✅ cycle |
| `collapseCompleted` | on/off | off | ✅ cycle |
| `showAll` | on/off | off | ✅ cycle |
| `maxVisible` | 5–100 | 10 | ✅ cycle |
| `sortOrder` | 预设名 或 排序 spec | `id` | ⚠️ 自定义 spec 只读（见 §3.3） |
| `hiddenAt` | top/bottom | bottom | ✅ cycle |
| `glyphs` | glyph 集 | 内置 | ➖ config-file only（与上游一致） |

**面板即写盘**：上游设置面板 onChange 立即 `saveTasksConfig` 且**只写与全局不同的项**。
prux 的 `apply_setting` 应同样只落差异项（对齐 `subagent/config.rs::apply_panel_choice` 的做法）。

### 2.7 与 `subagent` 扩展的联动（协议对照）

**这是本迁移最高优先级、也是风险最低的部分**：prux 的 subagent 扩展在 subagent 迁移的
S6c/Tier4 阶段就已实现 `subagents:rpc:*` 的**协议 v2**，与 pi-tasks 的期望完全同构。
逐通道对照如下：

| 方向 | 通道 | pi-tasks 期望 | prux subagent 现状 | 结论 |
|---|---|---|---|---|
| 发送 | `subagents:rpc:ping` `{requestId}` | 收 `subagents:rpc:ping:reply:<id>` `{success,data:{version}}` | `events::on_event` → `reply(... {version: PROTOCOL_VERSION=2})` | ✅ 就绪 |
| 接收 | `subagents:ready` | 无 payload，触发重新 ping | `events::ready()`（子代理 `on_session_start` / 启用时广播） | ✅ 就绪 |
| 发送 | `subagents:rpc:spawn` `{requestId,type,prompt,options}` | `options = {description,isBackground:true,maxTurns,model?}`，回 `{success,data:{id}}` | `rpc_spawn` 读取 `type`/`prompt`/`options.description`/`options.maxTurns`/`options.model`；**忽略 `isBackground`（恒后台）** | ✅ 兼容（`isBackground` 冗余） |
| 发送 | `subagents:rpc:stop` `{requestId,agentId}` | 停子代理 | `rpc_stop`，仅顶层记录可停 | ✅ 就绪 |
| 发送 | `subagents:rpc:consume` `{requestId,agentId}` | fire-and-forget，抑制完成通知 | `rpc_consume`（`manager::consume_result`） | ✅ 就绪 |
| 接收 | `subagents:completed` `{id,result?}` | 任务标 completed、存 `metadata.result`、触发 cascade | `lifecycle_payload`：`{id,type,description,result,status,toolUses,durationMs,tokens?,usage?}` | ✅ 超集 |
| 接收 | `subagents:failed` `{id,error?,result?,status}` | `status=="stopped"` → 有意停止（标 completed、留部分结果）；否则 → 回退 `pending` + `lastError` | 对 `Error｜Stopped｜Aborted` 均发 `subagents:failed`；`status` ∈ `error`/`stopped`/`aborted` | ⚠️ **aborted 归类差异**，见 §3.9 |

**协议门控**：prux 总线要求发送方 `name()` 是"已注册且启用"的扩展（`event::emit` 的
`sender_is_live`），因此扩展名必须是稳定的 `"tasks"`；启用/禁用会直接影响能否发出 RPC。

**agentTaskMap 与 reload 重连**：上游用内存 `Map<agentId, taskId>` 做 O(1) 完成查找，
并在 reload 后从磁盘 `task.metadata.agentId` 重建（仅 `in_progress` 任务）。
prux 的 subagent 扩展在会话切换时会 `manager::reset_all()` 中止旧子代理，因此
"reload 重连"的适用场景比 pi 窄，但语义仍需保留（Tier3 §3.9）。

### 2.8 进程跟踪

上游 `ProcessTracker`（140 行）提供 `track` / `getOutput` / `waitForCompletion` / `stop`，
但 `index.ts` **从未调用 `track()`**——没有代码路径能把进程塞进 tracker。`TaskOutput` /
`TaskStop` 的进程分支因此永远是死分支，只有测试驱动。上游 README 把
"Background Bash auto-task creation" 列为 Future Work，并注明 pi 的 bash 工具没有
`run_in_background`；prux 的 `src/core/tools/bash.rs` 同样没有该参数。→ **Tier4 决策：不迁移**
（见 §3.11）。

---

## 三、行为偏离清单

### 3.1 Widget 载体：`aboveEditor` → dock 面板（🟢 已收敛）

- **上游行为**：`ui.setWidget("tasks", render, { placement: "aboveEditor" })` 在编辑器正上方
  渲染常驻块；用 `setInterval(150ms)` 自增 spinner 帧并请求重绘。
- **prux 行为**：prux 没有 "aboveEditor widget" 原语，任务列表映射为
  `ExtensionHook::Dock` 的 `dock_lines()`（状态栏上方的可滚动面板）。spinner 帧由
  `wants_redraw()` 请求每帧重绘、在 `dock_lines()` 内按 `now_ms()/150 % frames.len()` 计算，
  **无定时器**。
- **理由**：dock 是 prux 唯一的"常驻多行聚合面板"，subagent/FleetView/goal/plan-mode 都用它；
  新增 per-extension widget 位会与 dock 的布局/滚动模型重复。
- **用户可见差异**：任务列表出现在状态栏上方的 dock 区，而非紧贴输入框；dock 可被 `/dock`
  或 `ShowDock` 控制显隐，且与其它扩展的 dock 段堆叠（段间空行）。
- **收敛时机**：Tier2 采用。堆叠顺序由注册顺序（= 优先级）决定，已有测试钉住：
  goal(90) → plan-mode(85) → tasks(80) → subagent(78)，`tasks` 段紧跟 plan-mode（位于 subagent 之前），
  各段连续、互不合并（`core::extensions::dock::tests::sections_keep_registration_order`）。
  若用户坚持"紧贴输入框"，需新增核心原语（不在本迁移范围）。

### 3.2 缺少 `ui.input`：建任务改用带参子命令（🟢 已收敛）

- **上游行为**：`/tasks` → "Create task" 依次 `await ui.input("Task subject")`、
  `await ui.input("Task description")`，任一为空即返回菜单。
- **prux 行为**：核心的 UI 请求只有 `Select` / `Notify*` / `CustomMessage` / `ShowOverlay` /
  `ShowSettings` / `Continuation` 等；**没有单行输入 prompt**。可用 `OverlayView.input`
  （`OverlayInput`）或 `OverlayView.editor`（`OverlayEditor`，subagent `/agents edit` 已用）收集。
- **决定**：改用**带参子命令**，不引入 overlay 交互：
  `/tasks create <subject> [:: <description>]`。description 缺省时回退为 subject。
  子命令 `create` 已随 `commands()` 的 `subcommands` 声明（输入框 `@` 补全可见），
  与 handler 解析分支由 `subcommands_match_parser` 测试钉住。主菜单不再保留
  “Create task” 项（已由子命令取代，菜单项重复且只能提示用法）。

### 3.3 设置面板：`SettingsList` → 原生扩展设置面板（🟢 更优）

- **上游行为**：`ui.custom` 里构造 pi-tui `SettingsList`，8 项可循环 + 1 项只读。
- **prux 行为**：`Extension::settings()` 返回 `Vec<ExtensionSetting>`，`apply_setting(key, value)`
  应用并落盘；`/tasks settings` 发 `ShowSettings{ext:"tasks"}`。`ExtensionSetting::cycle`
  直接表达取值/标签/描述/候选。
- **偏离**：
  - `sortOrder` 的自定义 sort spec 与 `glyphs` 无法用离散循环表达 → 面板里**只读**展示
    （与上游 `custom` 只读、以及 `glyphs` config-file only 的处理一致）。
  - 面板由 TUI 原生渲染（占输入框区域、type-to-search），不再是 `ui.custom` 弹出块。
- **理由**：prux 有专门的面板通道，重复实现 `SettingsList` 无收益。
- **收敛**：Tier2，直接采用（更优，无需回退）。

### 3.4 瞬态提醒注入：`context` hook → `ContextWithSystem`（⚠️ 必须）

- **上游行为**：`pi.on("context")` 返回改写后的 messages；仅用于**本次请求**，不落盘。
- **prux 风险点**：prux 的 `transform_context`（`TransformContext` hook）**直接修改
  `agent.messages`**（`src/core/agent_loop.rs::prepare_model_context`），因此提醒会被写进
  会话历史——与上游语义不符，且每轮累积会污染 transcript。
- **正确接缝**：`ExtensionHook::ContextWithSystem` + `transform_context_with_system`。
  它拿到含 system 消息的完整 transcript，**结果原样发送、不改写 `agent.messages`**
  （`apply_context_with_system` 的文档与实现均保证）。
- **注意**：该钩子要求 `messages[0]` 保持 system（若提示非空）——只做 **append**，不要删改 `[0]`。
- **收敛**：Tier4 采用 `ContextWithSystem`。

### 3.5 会话与回合生命周期钩子形状不同（⚠️）

| 上游 | prux | 处理 |
|---|---|---|
| `session_start` 的 `reason`（`startup`/`new`/`resume`/`reload`/`fork`） | `on_session_start(cwd, messages, session)`（启动）+ `on_session_switched(path, messages)`（`/new`/`/resume`/`/import`/`/fork`/`/clone` 汇合） | **无 reason 枚举**：`path.is_none()` ≈ 全新会话（`/new`）；`Some(path)` 且 store 已有文件 ≈ resume；reload≈启动路径重跑。需按 path + store 状态推导，而不是读 reason。 |
| `agent_settled` | `on_boundary` `type=="agent_before_settle"` | 该边界**主/子 agent 都投**，需按 `event.agentScope == "main"` 过滤；`on_boundary` 的返回值为续跑决策，任务扩展应返回 `None`。 |
| `turn_end` | `on_boundary` `type=="turn_end"` | `event.message` 为 assistant 消息（含 `usage`）。 |
| `before_agent_start` | — | 惰性初始化改由 `on_session_start` + `on_exec_ctx` 承担。 |
| `tool_execution_start` | `on_agent_event` / `on_exec_ctx` | 刷新 `latest_ctx`；`on_exec_ctx` 在会话建立/每次 turn 开始/切换后投递。 |

### 3.6 路径与 agent 目录：`.pi` → `.prux`（🟢 直接改名）

- 项目态：`<cwd>/.pi/...` → `<cwd>/.prux/...`
- 全局态：`getAgentDir()`（`~/.pi/agent`）→ `settings_manager::agent_dir()`（`~/.prux`）
- `projectKey` 编码（`/Users/me/work/repo` → `--Users-me-work-repo--`）保持上游实现，
  与 prux 会话日志命名一致。
- 会话文件：`ctx.sessionManager.getSessionFile()` → `Session::get_session_file()`。

### 3.7 项目配置的信任门控（⚠️ 需拍板）

- **上游立场**：README/CUSTOMIZING 明确"Configuration is data, never code"，`.pi/` 里的配置
  **不执行任何代码**，因此不需要信任门控即可读取。
- **prux 现状**：`<cwd>/.prux/extensions/*.json` **受 `is_project_trusted` 门控**
  （subagent 的 `project_config_path()` 在未受信任时返回 `None`）。原因见
  `src/core/project_trust.rs`：项目文件不该在未受信任的仓库里悄悄改变 agent 行为。
- **两种落点**：
  1. `<cwd>/.prux/extensions/tasks.json`（受门控，符合 prux 惯例）；
  2. `<cwd>/.prux/tasks-config.json`（不受门控，纯数据，符合上游语义）。
- **建议**：采用 2。任务配置（尤其是 `autoCascade` / `taskScope`）不影响安全面，
  且上游明确"数据而非代码"；但需与用户确认，因为 prux 尚未有"不受门控的项目配置"先例。
- **未决**：见 §7。

### 3.8 跨进程文件锁实现（🔶 待定）

- 上游：`<file>.lock`，内容 `<pid>:<uuid>`，`wx` 独占创建，PID staleness 检查
  （`process.kill(pid,0)`）+ 未可读 PID 的二次轮询，释放前比对 token（防被跨 PID
  namespace 抢占后误删）。
- prux：依赖里无 `fs2`/`fs4`。可选：
  1. 移植上游 lock-file 协议（可跨平台、行为等价、易测）；
  2. 用 `nix::fcntl::flock`（unix 依赖已含 `fs` feature）——更省事但换掉 staleness 语义；
  3. 新增 `fs4` 依赖（需用户同意加依赖）。
- **建议**：1（零新依赖、行为可复刻、已有上游测试可对照）。
- **收敛**：Tier4。

### 3.9 `aborted` 状态归类（⚠️ 必须处理）

- **上游行为**：只把 `status === "stopped"` 视为"有意停止"（标 completed、保留部分结果），
  其余非 completed 状态一律回退 `pending` + `lastError`。
- **prux 行为**：subagent 的 `settled()` 对 `Error｜Stopped｜Aborted` **都**发
  `subagents:failed`，`status` 分别为 `error`/`stopped`/`aborted`。其中 `Aborted` 对应
  "到达 `max_turns` 且宽限期用尽被停机"，`Stopped` 对应"用户主动中止"。
- **偏离**：照搬上游会把 `aborted` 当错误回退 pending，而它其实是"预算耗尽"的**终态**。
- **建议**：把"有意停止"集合显式化（`stopped` + `aborted`，或按是否 `result` 非空判定）。
  这是**唯一一处需要主动改写上游逻辑**的联动点，务必在迁移时记账。

### 3.10 token 用量归属（⚠️ 语义提升点）

- **上游行为**：从主会话 `turn_end` 的 assistant `message.usage` 取数，并**分摊给所有
  当前 active 任务**。这是"主 agent 在任务运行期间的开销"，并非子代理真实开销。
- **prux 可选改进**：subagent 的 `subagents:completed` payload 已带 `tokens` / `usage`
  （见 `lifecycle_payload`），子代理运行的 token 可从事件精确获取。
- **建议**：Tier2 先照搬上游（保持 UI 行为一致），Tier3 评估改用事件用量并在上方文案标明
  口径。无论哪种，**不要**在主/子事件里重复计数。

### 3.11 `ProcessTracker` 不迁移（➖）

- 理由见 §2.8：无生产者。
- **动作**：不建 `tasks/process.rs`；`TaskOutput` / `TaskStop` 只保留子代理分支（工具名与参数
  不变，模型无感）。若未来 prux 的 bash 工具加入 `run_in_background`，再按上游 README 的
  Future Work 重新评估。

### 3.12 `/tasks` 多级菜单：递归 await → 状态机（⚠️）

- **上游**：`ui.select` 是 await 的，天然写递归函数。
- **prux**：`Select` 通过 `on_ui_choice(id, choice)` 异步回传，是"发请求 → 存上下文 → 回传时
  按 id 匹配"的事件驱动模型。多级菜单需要一个显式状态机（可完全照搬 subagent 的
  `SelectKind` + `select_id_slot` 模式：`src/extensions/subagent.rs`）。
- **注意**：`on_ui_choice` **不得加 agent 锁**（TUI 线程调用）。

### 3.13 fork / clone 的任务继承（🟢 已收敛）

- **上游**：`session_start` 的 `reason === "fork"` 时，`store.snapshot()` 取父会话任务，
  切换 store 后 `seed()` 灌入 fork 文件（`seed` 对非空 store 是 no-op，避免重复）。
- **prux**：`on_session_switched(path, messages)` 只给**新**会话路径，拿不到父会话路径。
- **实现**：prux 的 fork 会话文件头带 `parent_session_id`（`Session::fork_from/fork_at` 写入）。
  `on_session_switched` 只读新会话文件首行取回父会话 id，**仅当父会话正是刚离开的会话时**
  （`/fork` 与 `/clone` 都从当前会话复制）才把父会话任务种入新 store。`/resume` 一个旧的
  fork 不会误触发；`project`/`memory`/`PRUX_TASKS` 显式路径不按会话分文件，不继承。
  `session_parent_id` 只读首行，避免为取文件头而整解析大型会话。
- **测试**：`fork_header_exposes_parent_session_id`（文件头契约）、`fork_seed_inherits_parent_tasks`、
  `fork_seed_skips_non_session_scopes`。

### 3.14 环境变量改名（🟢）

| 上游 | prux | 说明 |
|---|---|---|
| `PI_TASKS` | `PRUX_TASKS` | `off` / 具名 / 绝对路径 / 相对路径 四种取值 |
| `PI_TASKS_DEBUG` | `PRUX_TASKS_DEBUG` | 输出 RPC 请求/回执/超时与 spawn 错误 |

对齐 prux 既有 `PRUX_AGENT_DIR` 命名惯例。

### 3.15 提醒文案的渲染可见性（🟢 已验证，无需处理）

`<system-reminder>…</system-reminder>` 作为 user 消息经 `ContextWithSystem` 注入。
关键在于：该钩子的结果**只用于本次 LLM 请求、不写回 `agent.messages`**
（`src/core/agent_loop.rs::apply_context_with_system`），而 UI 渲染的是 `agent.messages`。
因此提醒根本不进入 markdown 渲染管线，不存在“尖括号被当 HTML 处理”或“被暴露成正文”的风险。

回归测试 `reminder_injection_appends_without_mutating_transcript` 钉住：注入只
append 一条 user 消息，`<system-reminder>` 标签原样保留，既有 transcript 一字不改，
且 drain 后不重复累积。

---

## 四、用户迁移动作

从 pi/pi-tasks 切到 prux 的 `tasks` 扩展时，用户需要：

1. **开启扩展**：`tasks` 建议 `default_enabled = false`（主动注入 7 个工具、改变 agentic 行为，
   与 plan-mode / goal / subagent 一致）。在 `/extension` 面板空格开启，或写 `settings.json`
   的 `enabledExtensions`。
2. **迁移任务数据**（可选）：
   - `.pi/tasks/tasks-*.json` → `.prux/tasks/tasks-*.json`
   - `.pi/tasks/tasks.json` → `.prux/tasks/tasks.json`
   - `~/.pi/agent/tasks/sessions/<key>/` → `~/.prux/extensions/tasks/sessions/<key>/`
3. **迁移配置**（可选）：
   - `<workspace>/.pi/tasks-config.json` → `<workspace>/.prux/tasks-config.json`
   - `~/.pi/agent/tasks-config.json` → `~/.prux/tasks-config.json`
4. **改名环境变量**：`PI_TASKS` → `PRUX_TASKS`，`PI_TASKS_DEBUG` → `PRUX_TASKS_DEBUG`。
5. **子代理联动**：prux 的 `subagent` 扩展需已启用（同一 `enabledExtensions` 机制）；
   协议 v2 一致，**无需**额外版本对齐，但注意 §3.9 的 `aborted` 归类。
6. **`.gitignore`**：若用默认 `session` 作用域，仍需忽略 `.prux/tasks/`（与上游 `.pi/tasks/` 等价）；
   改用 `session-global` 可让仓库保持干净。

---

## 五、核心改动清单（契约冻结）

**预期：零核心改动。** 本扩展只消费既有接缝：

| 复用接缝 | 位置 | 用途 |
|---|---|---|
| `Extension` trait（tools / commands / hooks / settings / dock_lines / wants_redraw / on_agent_event / on_boundary / transform_context_with_system / on_session_start / on_session_switched / on_extension_event / execute_tool_async / on_ui_choice / on_registered） | `src/core/extensions.rs` | 全部扩展行为 |
| 扩展事件总线 `events::emit` / `on_extension_event` | `src/core/extensions/events.rs` | 与 subagent 的 RPC |
| `ExtensionUiRequest::{Select, NotifyRich, CustomMessage, ShowOverlay, ShowSettings, PersistSessionEntry}` | `src/core/extensions/ui.rs` | 菜单、通知、设置面板 |
| `DockLine` / `DockSpan` | `src/core/extensions/dock.rs` | 任务列表 |
| `ExtensionHook::{ContextWithSystem, Boundary, AgentEvent, AfterToolCall, Dock, ExtensionEvent}` | `src/core/extensions/hooks.rs` | 提醒注入、生命周期、cadence |
| `Session::get_session_file()` / `append_custom_entry` | `src/core/session_manager.rs` | 会话落盘与恢复 |
| `settings_manager::agent_dir()` | `src/core/settings_manager.rs` | 全局配置/任务目录 |
| subagent 扩展的 `subagents:rpc:*` 与 `subagents:*` 事件 | `src/extensions/subagent/events.rs` | 联动 |

**唯一需要动的"周边"文件**（非核心）：
- `src/extensions.rs`：加 `pub mod tasks;` 与优先级常量 `PRIORITY_TASKS`
  （建议 78——在 subagent 的 80 之后注册，保证任务扩展在 subagent 广播 `subagents:ready`
  之前已注册；注册顺序只影响分发顺序，不影响投递）。
- `Cargo.toml`：仅当选择 `fs4` 做文件锁时需新增依赖（§3.8 建议避免）。

**门面 trait 实现要点**（供实施时对照）：
```rust
fn name(&self) -> &str { EXT }                       // EXT = "tasks"（总线发送方身份）
fn fork_project(&self) -> Option<ForkProjectInfo> {
    Some(ForkProjectInfo { plugin_name: "pi-tasks".into(), plugin_version: "0.9.0".into(),
                           url: "https://github.com/tintinweb/pi-tasks".into() })
}
fn modes(&self) -> Vec<ExtensionMode> { vec![Dev, Creator] }
fn default_enabled(&self) -> bool { false }
```

---

## 六、测试对照表

上游 21 个测试文件（6 181 行）→ prux 侧落点：

| 上游测试 | 行数 | prux 对照 |
|---|---|---|
| `task-store.test.ts` | 630 | `tasks/store.rs` 单元测试（CRUD、依赖边、警告、信封校验、规范化） |
| `task-store-concurrency.test.ts` | 226 | 跨进程锁与并发写（多线程/多进程） |
| `store-scope.test.ts` | 290 | 四种作用域落点与切换（不搬移/不 stranding） |
| `task-paths.test.ts` | 100 | `projectKey` 编码、`session-global` 优先命中 workspace 文件 |
| `tasks-config.test.ts` | 185 | 两层合并、`glyphs` 深合并、只写差异项 |
| `task-sort.test.ts` | 131 | 预设与 spec、非法输入回退 |
| `task-glyphs.test.ts` | 143 | 逐项回退、控制字符/双向覆写拒绝 |
| `reminder-cadence.test.ts` | 126 | cadence 纯函数 |
| `stale-task-reminder.test.ts` | 222 | `turn_end` 滞留检测 → `ContextWithSystem` 注入 |
| `auto-clear.test.ts` + `auto-clear-lifecycle.test.ts` | 694 | `auto_clear.rs` 两种模式 + 批次边界 |
| `task-widget.test.ts` | 893 | `widget.rs` 行构建（glyph/spinner/溢出/折叠/截断） |
| `tasks-command.test.ts` | 225 | `/tasks` 状态机（Select + on_ui_choice） |
| `subagent-integration.test.ts` | 1 204 | RPC 握手/派发/完成/失败/cascade/reattach/消费 |
| `subagent-result-consumption.test.ts` | 127 | `TaskOutput` 交付结果后 `consume` |
| `auto-cascade.test.ts` | 172 | DAG 级联（依赖结果注入 prompt） |
| `agent-reattach.test.ts` | 201 | reload 后从 `metadata.agentId` 重连 |
| `task-output-stop.test.ts` | 249 | `TaskOutput` 阻塞/超时、`TaskStop` |
| `process-tracker.test.ts` | 178 | ➖ 不迁移（对应代码不迁移） |

**prux 侧必须新增的用例**（上游没有对应物）：
- `ContextWithSystem` 注入后 **`agent.messages` 不被改写**（回归保护，§3.4）。
- `on_boundary` 的 `agentScope == "subagent"` 事件**不**触发 auto-clear 的 run 边界（§3.5）。
- `subagents:failed` 的 `status == "aborted"` 归类（§3.9）。
- 项目配置在未受信任仓库下的可见性（取决于 §3.7 的决策）。
- 总线下 `tasks` 扩展被禁用时无法发出 RPC（`sender_is_live` 门控）。
- `/tasks create` 参数解析与命令入口接线（§3.2）：`parse_create_args_forms` /
  `command_create_builds_task_with_description`。
- fork seed（§3.13）：`fork_header_exposes_parent_session_id` / `fork_seed_inherits_parent_tasks` /
  `fork_seed_skips_non_session_scopes`。
- dock 堆叠顺序（§3.1/#9）：`core::extensions::dock::tests::sections_keep_registration_order`。

---

## 七、已知缺口与未决项

| # | 项 | 类型 | 处理 |
|---|---|---|---|
| 1 | 项目配置落点（受信任门控的 `.prux/extensions/tasks.json` vs 纯数据 `.prux/tasks-config.json`） | **需用户拍板** | §3.7，建议后者 |
| 2 | ~~`/tasks` 建任务的输入交互~~ | 已解决 | §3.2：改用 `/tasks create` 带参子命令 |
| 3 | ~~`/fork` 时任务继承的时序~~ | 已解决 | §3.13：读会话文件头的 `parent_session_id` |
| 4 | `aborted` 状态归类（上游会误判为错误回退） | 需主动改写 | §3.9 |
| 5 | 跨进程文件锁实现（移植 lock-file vs `nix::flock` vs 新增 `fs4`） | 待定 | §3.8，建议移植 |
| 6 | token 用量口径（主会话分摊 vs 子代理事件精确值） | 语义提升 | §3.10 |
| 7 | ~~`<system-reminder>` 在 prux 渲染管线的可见性~~ | 已验证 | §3.15：不进渲染管线 |
| 8 | ~~`TaskList`/`TaskCreate` 是否声明 `ToolExecutionMode::Parallel`~~ | 已解决 | §2.1：`TaskCreate` 已声明 Parallel |
| 9 | ~~dock 段与其它扩展（subagent/goal/plan-mode）的堆叠顺序与多段可读性~~ | 已验证 | §3.1：注册顺序测试钉住 |

---

## 八、进度勾选表（实施完成后回填）

### 8.1 Tier1 任务内核 ✅
- [x] `tasks/types.rs`：`Task` / `TaskStatus` / `TaskStoreData` / `TaskUpdateFields`
- [x] `tasks/store.rs`：内存 + 文件存储、`with_lock`、原子写（tmp+rename）、信封校验、逐记录归一化
- [x] `tasks/store.rs`：依赖双向边 + 自依赖/环/悬空警告 + 删除清理
- [x] 工具：`TaskCreate` / `TaskList` / `TaskGet` / `TaskUpdate`（schema 与文案照抄上游）
- [x] `/tasks`：View all / Clear completed / Clear all（Select + `on_ui_choice` 状态机）
- [x] `tasks.rs` 门面：`name` / `fork_project` / `modes` / `default_enabled` / `tools` / `commands` / `on_registered` / `execute_tool_async`

### 8.2 Tier2 Widget 与设置 ✅
- [x] `tasks/glyphs.rs` / `tasks/sort.rs`：解析与回退
- [x] `tasks/widget.rs`：头部汇总、状态 glyph、blocked 后缀、溢出、折叠
- [x] spinner 帧（墙钟驱动）+ elapsed + token 指标
- [x] `wants_redraw()` + `request_show_dock()` + 空列表自动隐藏
- [x] `tasks/config.rs`：两层合并、`glyphs` 深合并、差异写回
- [x] `settings()` / `apply_setting()` / `/tasks settings` → `ShowSettings`
- [x] `tasks/auto_clear.rs`：`never` / `on_list_complete` / `on_task_complete` + 批次边界
- [x] 生命周期：`on_session_start` / `on_session_switched` / `on_boundary(turn_end, agent_before_settle)` / `on_agent_event(turn_start)`
- [x] 建任务：`/tasks create <subject> [:: <description>]` 带参子命令（不再用 overlay，见 §3.2）
- [x] `subcommands` 声明 + 与 handler 解析分支一致性测试

### 8.3 Tier3 subagent 联动 ✅
- [x] `tasks/subagent.rs`：ping/ready 握手（`PROTOCOL_VERSION = 2`）+ 版本告警
- [x] `TaskExecute`：校验 + `spawn` RPC + `agent_task_map` + `metadata.agentId`
- [x] `TaskOutput` / `TaskStop`：子代理分支 + 结果消费（`consume` RPC）
- [x] 完成/失败监听：completed → 标 completed + 存结果；failed → 按 §3.9 归类（stopped + aborted 均视为有意停止）
- [x] auto-cascade：未阻塞依赖注入 prompt + 级联派发
- [x] `reattach`：会话切换/恢复时从磁盘重建 `agent_task_map`（仅 in_progress）
- [x] 回归：aborted 归类、agentScope 过滤、pending 不重连

### 8.4 Tier4 环境与收尾 ✅（`PRUX_TASKS_DEBUG` 除外）
- [x] 四种 `taskScope` + `session-global` 的"新文件落点"语义 + 目录回收
- [x] `PRUX_TASKS`（off / 具名 / 绝对 / 相对）；`PRUX_TASKS_DEBUG` ➖ 未接（见 §9.4）
- [x] `tasks/cadence.rs` + `transform_context_with_system` 注入
- [x] 跨进程文件锁（移植 lock-file 协议，原子化改造见 §9.1）
- [x] fork seed（读会话文件头 `parent_session_id`，见 §3.13）
- [x] 测试：§6 的 prux 新增用例（aborted 归类 / pending 不重连 / cadence / 提醒回显 / 锁并发）
- [x] 验证：§3.1 dock 堆叠顺序、§3.15 提醒瞬态注入、§3.2/§3.13 新功能回归

---

## 九、实施记录（实际偏离与发现）

### 9.1 文件锁必须「带内容原子出现」（修正上游实现细节）

上游用 `writeFileSync(lockPath, token, {flag:"wx"})`：open 创建与 write 是两个动作，
其间存在一个「锁文件已存在但为空」的窗口。上游靠「无 PID 的锁等两轮再判陈旧」兜底，
在 Node 里窗口足够窄。Rust 侧实测（8 线程并发写同一文件）该窗口会被反复命中：
等在外面的获取者读到空内容 → 判定陈旧 → 删掉持有者的锁 → 多个会话同时进入临界区
（并发深度可达 4，`tasks/store.rs` 的并发测试抓到）。

**修正**：先把 token 写进同目录临时文件，再 `hard_link` 到锁路径（目标存在则失败）。
锁文件因此**带内容原子出现**，空内容只可能来自崩溃/外部损坏，两轮兜底恢复其原意。
验证：`concurrent_updates_are_locked`（8 线程 → 恰好 8 条任务）。

### 9.2 `/tasks` → Create task：采用带参子命令（不引入 overlay）

prux 无 `ui.input` 单行输入原语。最终采用**带参子命令**而非 overlay：
`/tasks create <subject> [:: <description>]`。`create` 已随 `commands()` 的 `subcommands`
声明（输入框补全可见），主菜单的 `Create task` 项已移除（建任务只经子命令，避免
菜单项与子命令两套入口）。
`parse_create_args` / `subcommands_match_parser` / `command_create_builds_task_with_description`
钉住解析与接线。subagent 的 `/agents edit` 用的 `OverlayEditor` 仍然可用，但本流程不再依赖它。

### 9.3 fork 的任务继承：读会话文件头（已接线）

prux 的 `on_session_switched(path, messages)` 只给**新**会话路径。实现改用新会话文件头里的
`parent_session_id`（`Session::fork_from` / `fork_at` 写入）：只读文件首行（`session_parent_id`，
避免整解析大型会话），仅当父会话正是刚离开的会话时才 `TaskStore::seed`。`snapshot` / `seed`
上的 `#[allow(dead_code)]` 已移除。测试：`fork_header_exposes_parent_session_id` /
`fork_seed_inherits_parent_tasks` / `fork_seed_skips_non_session_scopes`。

### 9.4 `PRUX_TASKS_DEBUG` 未接

上游的 RPC 调试输出只在 JS 侧有意义；prux 的扩展没有统一的 stderr 调试通道。
`PRUX_TASKS`（存储覆盖）已完整实现；调试变量留待需要时再定去向（日志或 `eprintln!`）。

### 9.5 token 用量口径照搬上游

`on_boundary(turn_end)` 读**主会话** assistant 的 `usage`，分摊给当前 active 任务。
这是「主 agent 在任务运行期间的开销」，并非子代理真实开销；`subagents:completed`
payload 已带精确 `tokens`/`usage`，后续可切换（§3.10）。

### 9.6 观察到的既有失败（与本次迁移无关）

`extensions::footer::rich::tests::llm_calls_count_turn_starts_on_line2_right` 与
`session_new_resets_all_accumulated_state` 在本仓库 HEAD 上**已失败**（已用「去掉
`pub mod tasks;` 后单独运行」验证为预先存在，与本扩展无关）。
`cargo test --lib extensions::` 除这两项外全绿（576 passed）。
