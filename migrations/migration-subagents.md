# Prux 迁移指南：pi-subagents → prux 扩展 `subagent`

> **来源**：`@tintinweb/pi-subagents` v0.19.0（`pi-subagents/`，独立 git 检出）
> **目标**：`src/extensions/subagent.rs`（重写）+ `src/extensions/subagent/`
> **性质**：本文档是先写后做的**契约冻结版**——第 1–5 章在动代码前定稿，第 2/8 章的
> 状态列在实施过程中就地回填。与 `migration.md`（pi 0.84→0.85 版本升级）不同，
> 本文档记录的是**一个上游插件整体移植进 prux 扩展体系**的映射、偏离与缺口。
>
> **更新记录**：
> - `Tier1 设计冻结`：第 0–7 章定稿，状态列待实施回填。
> - `Tier1 实施完成`：核心三处接缝 + 扩展五个模块落地；第 2/6/7/8 章回填，
>   第 5.4 节记录实施中的 4 处契约修正与 1 处能力补强，第 8.5 节记录实施期发现的
>   既有问题及处理。
> - `Tier2 切片 1 完成`：设置层（`subagent.json` + 设置面板）、常驻 widget（多行 +
>   spinner + 颜色 badge）、`/agents` 一/二级菜单、默认后台翻转、`NotifyRich` 结果卡片、
>   测试互斥统一与 env 无关化。
> - `Tier2 完成`：核心新增**覆盖层原语**（`OverlayView` / `on_overlay_event`）与
>   **自定义消息卡片**（`CustomMessage` + `render_custom_message`）→ FleetView、
>   会话查看器（实时转写 + 内联 steer/停止/续跑）、完成卡片（含 `/resume` 重放）、
>   join 分组（smart/group/async）。见 §5.5 / §8.7。
> - `Tier3 选型结案`：JS 引擎 = `rquickjs 0.12.2`，上游示例脚本原样跑通（见 §7.1）。
> - `后续切片路线`：§9（S2→S6，用户已确认顺序与保真度策略）。
> - `S2 第一段完成`：设置项扩到 14 项（含 `tool_description`/`fallback_subagent`/
>   `strict_agent_files`/`disable_default_agents`/`output_transcript`）、描述三模式、
>   `disallowed_tools`、`isolated`（核心 spec 过滤）、`.output` 运行转写、
>   `/agents eject|enable|disable`（逐行 frontmatter 编辑）、frontmatter 策略锁定
>   （`run_in_background`/`inherit_context`）。S2 剩余：扩展作用域（`ext:`/`extensions`/
>   `exclude_extensions`）、`skills:` 预载、`@mention`、`isolated` 工具参数。
> - `S2 第二段完成`：扩展作用域（`extensions`/`exclude_extensions` + `tools:` 的 `ext:` 选择器）、
>   `skills:` 预载与 `skills: false`、`isolated` 工具参数。
> - `S2 第三段完成（S2 收尾）`：核心「用户输入拦截」钩子（`ExtensionHook::UserPrompt` +
>   `on_user_prompt` → `Continue`/`Rewrite`/`Handled`，统一原 `on_user_input`）与会话执行
>   上下文钩子（`on_exec_ctx`）；`@mention` 路由（`@main`/`@agent-`/steer/resume/类型
>   spawn，见 §5.6）。**S2 完成**。
> - `S3 第一刀完成`：Tier3 执行内核落地（`rquickjs` 沙箱 + 桥 + `agent/parallel/pipeline/`
>   `phase/log/args/budget` + `meta` 校验 + 配额 + `SubagentWorkflow` 工具 + 完成卡片，
>   见 §8.9）；结构化输出为近似、后台语义待补。
> - `Tier 状态审计与收口`：逐行核对上游 56 个文件与 §2 特性表，把**已交付但状态陈旧**
>   的行（§1.1 的 🔶、row 29/70/82 等）回填；把**仍未实现**的项集中到 §11
>   （工作流 `isolation` / 嵌套 `workflow()` / 未 await 检测、3 项扩展设置、生成式创建
>   向导、join 批量合计），并把 §3.7 / §3.13 的过时描述改成实际结论。
> - `S7 完成（剩余项清零）`：上一轮 §11.1 的 7 个真实缺口全部补齐：工作流
>   `agent({isolation})`、嵌套 `workflow()`（一层 + 256 上限）、未 await 启动检测、
>   设置 `fleetView`/`rememberAgents`/`viewerMarkdown`、设置
>   `defaultMaxTurns`/`graceTurns`、向导 “Generate with Claude”、join 批量 Σ 合计。
>   见 §9 的 S7 行与 §11。
> - `S7b 复审补齐`：再次对照上游 README/类型逐项比对，又发现 4 处契约缺口并收口：
>   ① 嵌套委派改为**按 `allowed_subagents` opt-in**（此前按深度默认开启）；
>   ② `serialize_agent_file` 补齐 `allowed_subagents`/`extensions`/`exclude_extensions`/
>   `skills`/`memory`/`isolation`（eject 不再丢字段）；③ 自定义工具描述补齐上游占位符
>   `{{agentDir}}`/`{{compactTypeList}}`/`{{isolationGuideline}}`/`{{scheduleGuideline}}`；
>   ④ 工作流里真失败的子代理交回 `null`（此前会交回空字符串）。见 §11.5。
> - `S7c 全量复审`：对照上游 README `Features`/`Custom Agents`/`Tools` 逐条比对，又补齐 3 处：
>   ① 工作流沙箱禁用 `eval` / `new Function`（对齐 `codeGeneration: {strings:false}`）；
>   ② `/agents types` 改为**交互二级菜单**并新增 `delete`/`reset` 子命令（上游 Delete/Reset
>   to default）；③ 嵌套子代理的 token 花费**上卷到父记录**。
>   见 §11.7。
> - `S7d 全量复审`：补上剩余两项——① 核心新增**多行编辑器原语** `OverlayEditor`，
>   类型管理新增 `Edit`（`/agents edit` + 二级菜单，整文件编辑）；手动向导新增
>   `System prompt` 多行步；② 工作流移植上游 `checkBoundary`（循环/非有限数/undefined/
>   BigInt/符号/函数/稀疏数组/非纯对象）并对 args/返回值加体积上界。见 §9 的 S7d 行与
>   §11.8。

---

## 〇、摘要与 Tier 路线图

### 上游规模

| 维度 | 数值 |
|---|---|
| TS 源码 | 20 929 行 / 56 个文件（`src/` 34 + `src/ui/` 10 + `src/workflow/` 12） |
| 最大单文件 | `src/index.ts` 3 988 行（工具注册 + `/agents` 菜单 + 渲染） |
| 测试 | 97 个 `*.test.ts` + `test/perf/` 基准 |
| 文档 | `README.md`（104 KB）、`docs/workflows.md`、`docs/rpc.md` |
| 依赖 | `croner`（cron）、`nanoid`（id）、`typebox`；`node:vm`（工作流沙箱） |

### 迁移原则

1. **机制与策略分离**：prux 核心只提供"按 spec 物化一个隔离 child 并交回控制句柄"的机制；
   并发排队、后台生命周期、通知、UI 全是扩展的策略（详见第 1.3、5 章）。
2. **契约对齐优先于实现对齐**：模型可见的工具名、参数、状态词、frontmatter 字段名照抄上游；
   内部实现按 prux 风格重写，不做逐行翻译。
3. **故意偏离必须留痕**：每一处偏离在第 3 章记录"上游行为 / prux 行为 / 理由 / 用户动作 / 收敛时机"。
4. **Tier 化收敛**：每个 Tier 独立可验收，且不改变前一层已冻结的接口形状。

### Tier 路线图

| Tier | 主题 | 内容 | 状态 |
|---|---|---|---|
| **Tier1** | 执行内核 | agent 类型加载（两层）+ 默认三类型 + `Agent` / `get_subagent_result` / `steer_subagent` + 后台管理器（并发 10 + FIFO 排队 + 结果表）+ graceful max_turns + resume（子会话落盘并挂父会话）+ `inherit_context` + `/agents` 极简命令 + dock 汇总行 | ✅ 已实施（含 4 处契约修正，见 5.4） |
| **Tier2** | UI | 常驻 widget、FleetView、会话查看器、`/agents` 完整菜单、颜色 badge、消息卡片渲染、join 分组（smart/30s 批量） | ✅ 已实施（widget / FleetView / 查看器 / 菜单 / 颜色 / 卡片 / join；查看器滚动键已接 keybindings，row 82） |
| **Tier3** | 工作流 | `SubagentWorkflow` + JS 沙箱 + 进度卡片 + 保存/重放 + `meta` 校验 | ✅ 已实施（引擎选型 rquickjs，见 §7.1；S3–S4d，§8.9–§8.15） |
| **Tier4** | 环境与集成 | worktree 隔离、持久记忆、skills 预载、`disallowed_tools`、`isolated`、extension scoping、schedule、`@mention` + mention-clone、跨扩展 RPC/事件总线、model scope、fuzzy model 解析、eject/agent-file 编辑、`strictAgentFiles`、`.output` transcript、自定义工具描述、设置面板全量项 | ✅ 已实施（S2/S5/S6/S7；有意偏离见第 3 章与 §11.2；明确不做见 §11.3） |

**Tier1 明确不做**的项：nested 子代理、`isolated`、`isolation: worktree`、memory、skills 预载、
`disallowed_tools`、schedule、`@mention`、`SubagentWorkflow`、跨扩展 RPC/事件总线、
FleetView/widget/会话查看器、model scope、fuzzy model 解析、eject、`strictAgentFiles`、
分组 join、`.output` transcript、自定义工具描述文件。

### 状态取值

| 标记 | 含义 |
|---|---|
| ✅ | 已实现（第 2 章的 Tier 列标明所属层） |
| 🔶 | 待实现 |
| ➖ | 决策不迁移（附理由） |
| ⚠️ | 已实现但有偏离（见第 3 章） |
| 🟢 | Tier2 已实施，偏离已收敛 |

---

## 一、架构对照

### 1.1 上游模块 → prux 落点

上游按职责分四组。下表是全部 56 个文件的归宿；Tier1 只涉及加粗行。

#### A. 类型注册（发现 → 解析 → 提示词）

| 上游文件 | 行数 | 职责 | prux 落点 |
|---|---|---|---|
| **`types.ts`** | 377 | `AgentConfig`/`AgentRecord`/`AgentStatus`/`SubagentType` 等类型 | **`subagent/types.rs`** |
| **`default-agents.ts`** | 126 | 内嵌默认三类型 | **`subagent/agent_types.rs`**（Rust 常量） |
| **`custom-agents.ts`** | 324 | 从 `.pi/agents/`、`.agents/agents/`、全局加载 `.md` | **`subagent/agent_types.rs`**（两层：`.prux/agents/` + `<agent_dir>/extensions/agent/`） |
| **`agent-types.ts`** | 346 | 统一注册表 + 工具名解析 + fallback | **`subagent/agent_types.rs`** |
| **`prompts.ts`** | 142 | `buildAgentPrompt`：replace/append 两模式 + `<active_agent>` | **`subagent/prompt.rs`** + core `rebuild_ctx` |
| `agent-file-toggle.ts` | 269 | `/agents` 里启用/禁用/导出 agent 文件 | ✅ `subagent/agent_files.rs`（eject + 逐行 enable/disable）+ `subagent/wizard.rs`（手动创建向导） |
| `agent-color.ts` | 161 | Claude Code / Agency Agents 色名 → badge RGB | ✅ `subagent/widget.rs`（`resolve_agent_color`，widget / FleetView / 结果卡共用） |
| `skill-loader.ts` | 102 | skills 预载（三种布局） | ✅ `subagent/prompt.rs`（`preload_skills`） |
| `context.ts` | 58 | `inherit_context` 的父会话上下文 | ✅ **`subagent/manager.rs`**（Tier1 经 spec 的 `inherit_context`） |
| `structured-output.ts` | 130 | 工作流子代理的结构化输出工具 | ✅ `subagent/workflow/structured_output.rs`（S3b） |
| `xml.ts` | 13 | XML 转义 | ✅ **`subagent/notify.rs`**（本地需转义结果文本） |

#### B. 执行（spawn → run → collect）

| 上游文件 | 行数 | 职责 | prux 落点 |
|---|---|---|---|
| **`agent-runner.ts`** | 1 245 | 会话创建、执行、graceful max_turns、steer/resume | **core `agent_session.rs`（child 物化 + max_turns）** + **`subagent/manager.rs`** |
| **`agent-manager.ts`** | 1 581 | 生命周期、并发队列、完成通知 | **`subagent/manager.rs`** |
| **`invocation-config.ts`** | 155 | 工具参数 schema 与 frontmatter 权威性解析 | **`subagent/types.rs`** + `subagent.rs` 的 schema |
| **`status-note.ts`** | 90 | 异常结局的状态说明与部分结果抢救 | **`subagent/notify.rs`** |
| **`usage.ts`** | 167 | token 用量形状与累加器 | **`subagent/manager.rs`**（从 child 事件累加） |
| **`abortable.ts`** | 43 | 等待与 Esc 竞争（取消等待不取消 agent） | ✅ `subagent.rs` 的 `result_tool`：`wait` 与 `ctx.parent_abort` `select`（S6d / row 62） |
| `group-join.ts` | 141 | 分组完成通知 + 30s/15s 批量超时 | ✅ `subagent/manager.rs`（配置 `join` = smart/group/async） |
| `nested-tools.ts` | 422 | 交给子代理的属主作用域委派工具 | ✅ `subagent/nested.rs`（S6b；`allowed_subagents` 另见 row 28） |
| `child-context.ts` | 15 | `AsyncLocalStorage` 标记"为子会话工作" | ➖ 不适用（Rust 无 AsyncLocalStorage；身份改由 `ToolExecCtx.agent_id/depth` 显式传递） |
| `output-file.ts` | 155 | 流式写 `.output` transcript | ✅ `subagent/output_file.rs`（S2；布局偏离已由 S4a 收敛，见 §3.13） |

#### C. 调用面（模型/用户/其它扩展如何触发）

| 上游文件 | 行数 | 职责 | prux 落点 |
|---|---|---|---|
| **`index.ts`** | 3 988 | 工具/命令注册、`/agents` 菜单、渲染、事件 | **`subagent.rs`**（门面：`tools`/`commands`/`execute_tool_async`/钩子） |
| **`model-resolver.ts`** | 118 | 精确 `provider/modelId` + 模糊回退 | **`subagent/manager.rs`**（Tier1 只做**精确**解析） |
| `enabled-models.ts` | 180 | 读 pi 的 `enabledModels` | ✅ `subagent/model.rs`（复用核心 `read_enabled_models`） |
| `model-scope.ts` | 70 | scope 白名单策略 | ✅ `subagent/model.rs`（配置 `scope_models`） |
| `mention.ts` | 141 | `@handle message` 语法 | ✅ `subagent/mention.rs` + `subagent.rs::on_user_prompt` |
| `mention-clone.ts` | 196 | 用克隆会话跑 mention 回合 | ✅ `subagent.rs::mention_clone`（S6d，`agent_mentions=model`） |
| `cross-extension-rpc.ts` | 198 | 跨扩展 spawn/stop/consume | ✅ 核心 `core/extensions/events.rs` + `subagent/events.rs`（S6c） |

#### D. 调度、环境、设置

| 上游文件 | 行数 | 职责 | prux 落点 |
|---|---|---|---|
| `schedule.ts` | 386 | cron / `+10m` / interval / ISO 派发 | ✅ S5c（`schedule/`） |
| `schedule-store.ts` | 153 | PID 锁 + 会话级原子持久化 | ✅ S5c（会话级 + 原子写，无 PID 锁，见 8.17） |
| `memory.ts` | 179 | 持久记忆（project/local/user 三作用域） | ✅ `subagent/memory.rs`（S6d） |
| `worktree.ts` | 205 | git worktree 隔离与完成后提交 | ✅ `subagent/worktree.rs`（S6a） |
| `settings.ts` | 587 | `subagents.json` 全局/项目合并 + 事件 | ✅ `subagent/config.rs`（两层逐键合并，25 项） |
| `env.ts` | 33 | git/平台探测 | ➖ 无独立落点：git 探测并入 `subagent/worktree.rs`，prux 无平台分支需求 |

#### E. 工作流（Tier3）

| 上游文件 | 行数 | 职责 |
|---|---|---|
| `workflow/runtime.ts` | 1 219 | worker 生命周期、RPC 桥、信号量、上限、gate/resume |
| `workflow/worker-source.ts` | 781 | `node:vm` 沙箱：确定性前奏 + 脚本全局 |
| `workflow/progress.ts` | 550 | 进度事件日志与所有派生视图（纯函数） |
| `workflow/host.ts` | 403 | `WorkflowHost`（对 `AgentManager` 的适配） |
| `workflow/meta.ts` | 325 | `export const meta` 的提取与校验 |
| `workflow/task.ts` | 302 | `local_workflow` 任务记录与批量进度更新 |
| `workflow/saved.ts` | 217 | 具名工作流的发现与解析 |
| `workflow/tool-description.ts` | 200 | 面向模型的编排模式说明 |
| `workflow/journal.ts` | 164 | 运行日志（`resumeFromRunId` 重放） |
| `workflow/json-schema.ts` | 128 | `schema` 全局的校验 |
| `workflow/collisions.ts` | 123 | 与外部 `Workflow` 工具的让位检测 |
| `workflow/entry.ts` | 47 | 会话条目类型 |

#### F. UI（Tier2）

| 上游文件 | 行数 | 职责 |
|---|---|---|
| `ui/workflow-dialog.ts` | 1 115 | `/agents → Workflows` 双栏检查器 |
| `ui/agent-widget.ts` | 659 | 常驻 widget：spinner、活动、token、状态图标 |
| `ui/conversation-viewer.ts` | 589 | 实时代理会话浮层 |
| `ui/fleet-list.ts` | 543 | FleetView：编辑器下方可导航代理列表 |
| `ui/workflow-card.ts` | 470 | 内联工作流卡片 |
| `ui/agent-mention.ts` | 216 | `@` 名册与候选行 |
| `ui/workflow-menu.ts` | 193 | `/agents → Workflows` 菜单 |
| `ui/schedule-menu.ts` | 105 | `/agents → Scheduled jobs` |
| `ui/select-item.ts` | 45 | 防冲突的 `ctx.ui.select` 包装 |
| `ui/viewer-keys.ts` | 39 | 查看器滚动键（走用户 keybindings） |

### 1.2 prux 侧文件布局

```
src/extensions/subagent.rs              门面：linkme 工厂 + Extension impl
                                        （tools / commands / execute_tool_async / 钩子接线）
src/extensions/subagent/types.rs        AgentType / SpawnRequest / AgentRecord / AgentStatus / Usage
src/extensions/subagent/agent_types.rs  两层发现 + 手写 frontmatter 解析 + 内嵌默认三类型
                                        + 覆盖/禁用/告警 + 类型解析
src/extensions/subagent/manager.rs      注册表 + 并发池 + FIFO 排队 + abort + resume
                                        + 通知投递 + 用法累加 + 精确型号解析
src/extensions/subagent/prompt.rs       默认三类型提示词 + 子代理上下文桥接段
                                        + Agent 工具描述（含动态类型清单）+ 工具 snippet/guidelines
src/extensions/subagent/notify.rs       Continuation 信封 + 状态头 + 截断摘要 + verbose 转写
src/extensions.rs                       `pub mod subagent;`（已存在）+ PRIORITY_SUBAGENT（已存在）
```

**核心改动（恰好三处）**

```
src/core/extensions/tools.rs      SubAgentSpec / SubAgentControls / SubAgentRunner；ToolExecCtx 增两字段
src/core/agent_session.rs         按 spec 物化 child；max_turns 优雅收尾；前台 abort 联动；事件 sink 注入
src/core/session_manager.rs       SessionCreateOptions + Session::create_with（旧入口退化为薄包装）
```

### 1.3 核心接缝为什么必须存在

事实：扩展在 `execute_tool_async` 里只能拿到 `ToolExecCtx`（`cwd` + 若干闭包），
**拿不到 `&Agent` / `AgentTemplate`**，因此无法自行构造 child、也无法自行覆盖模型/工具/提示。
上游是包内直接 import `@earendil-works/pi-coding-agent` 的内部 API 来造 session 的；
prux 的扩展是编译期内置但被刻意限制在窄接口后（`web_access`/`goal`/`plan_mode` 均不碰 `Agent`）。

因此 Tier1 加厚接缝（而不是把子代理做成内置子系统，也不是让扩展直接持有 `Agent`）：

- **核心给机制**：`SubAgentSpec` → 物化隔离 child → 包成 `SubAgentRunner` trait object + `SubAgentControls`。
- **扩展给策略**：谁 spawn、何时 await、并发怎么排、结果怎么存、通知怎么发、命令怎么交互。
- **收益**：Tier4 往 child 上加 worktree / memory / skills 预载时，这些新增落在核心的物化逻辑里，
  不会散进扩展；`Agent` 的 40+ 字段不成为扩展的隐式契约。

---

## 二、逐特性总表

> 状态列语义见第 0 章。**Tier1 行的"prux 落点"是实施依据；🔶 行的落点是预告，可随 Tier 细化调整。**

### 2.1 类型注册与发现

| # | 特性 | 上游落点 | prux 落点 | Tier | 状态 |
|---|---|---|---|---|---|
| 1 | 自定义 agent 类型（YAML frontmatter + 系统提示正文） | `custom-agents.ts` | `agent_types.rs` | 1 | ✅ |
| 2 | 发现路径：项目 `.pi/agents/` > 项目 `.agents/agents/` > 全局 `<agentDir>/agents/` | `custom-agents.ts` | 收敛为两层：`.prux/agents/`（受项目信任门控）> `<agent_dir()>/extensions/agent/` | 1 | ⚠️ 见 3.2 |
| 3 | 内嵌默认三类型 `general-purpose` / `Explore` / `Plan` | `default-agents.ts` | `agent_types.rs`（Rust 常量） | 1 | ✅ |
| 4 | 默认类型可被同名 `.md` 覆盖 | `agent-types.ts` | `agent_types.rs` | 1 | ✅ |
| 5 | `enabled: false` 禁用任意类型（含默认） | `agent-types.ts` | `agent_types.rs` | 1 | ✅ |
| 6 | 类型名大小写不敏感 | `agent-types.ts` | `agent_types.rs` | 1 | ✅ |
| 7 | 未知/禁用类型回退 `general-purpose` + 说明 | `agent-types.ts` | `agent_types.rs` | 1 | ⚠️ 歧义时报错，见 3.3 |
| 8 | `fallbackSubagent: none` 严格模式 | `settings.ts` | 配置 `fallback_subagent`（`general-purpose`/`none`） | 4 | ✅ |
| 9 | `disableDefaultAgents` | `settings.ts` | 配置 `disable_default_agents`（不注册默认三类型） | 4 | ✅ |
| 10 | `strictAgentFiles`（坏文件 fatal） | `settings.ts` | 配置 `strict_agent_files`（坏文件 → 派发前报错，含路径清单） | 4 | ✅ |
| 11 | frontmatter 字段：`name`/`description`/`display_name` | `custom-agents.ts` | `agent_types.rs` | 1 | ✅ |
| 12 | frontmatter：`tools`（含 `ext:<扩展>` / `*` / `none` 选择器） | `agent-types.ts` | `expand_ext_selectors`：`ext:<扩展>` / `ext:<扩展>/<工具>` 展开成具体工具名；未知选择器原样保留 + 一次性告警 | 1→4 | ✅ |
| 13 | frontmatter：`model`（精确 + 模糊） | `model-resolver.ts` | `manager.rs`：Tier1 只做精确 `provider/modelId` | 1 | ⚠️ 见 3.3 |
| 14 | frontmatter：`thinking` | `agent-runner.ts` | core `set_thinking_level`（自动钳制） | 1 | ✅ |
| 15 | frontmatter：`max_turns` | `agent-runner.ts` | core（spec）+ graceful 收尾 | 1 | ✅ |
| 16 | frontmatter：`prompt_mode`（replace / append） | `prompts.ts` | core `rebuild_ctx` + `prompt.rs` | 1 | ✅ |
| 17 | `inherit_context`（工具参数 + frontmatter 均可，frontmatter 优先） | `context.ts` | spec → core 复制父 messages；frontmatter 已锁定（S2） | 1/4 | ✅ S2 |
| 18 | `run_in_background`（工具参数 + frontmatter 均可，frontmatter 优先） | `invocation-config.ts` | `manager.rs`（默认值见 3.1）；frontmatter 已锁定（S2） | 1/4 | ✅ S2 |
| 19 | frontmatter：`persist_session`（默认 `true`） | `agent-runner.ts` | spec → `SessionCreateOptions.persist` | 1 | ✅ |
| 20 | frontmatter：`session_dir` 覆盖 | `agent-runner.ts` | spec → `SessionCreateOptions.session_dir` | 1 | ✅ |
| 21 | frontmatter：`color`（badge 色） | `agent-color.ts` | `widget.rs`：色名/`#RRGGBB` → dock badge 着色 | 2 | 🟢 |
| 22 | frontmatter：`extensions` / `exclude_extensions` | `agent-types.ts` | 核心 spec `extensions`/`exclude_extensions`：按工具归属扩展过滤 child 的**工具面**（`extensions: false` 等价 `isolated`）；**钩子面未作用域化**（见 3.10） | 4 | ⚠️ 部分 |
| 23 | frontmatter：`skills` 预载/筛选 | `skill-loader.ts` | `prompt::preload_skills`：具名技能正文包成 `<preloaded_skills>` 注入 child 提示（复用核心 `/skill:` 展开器）；`skills: false` → 核心 `clear_skills` 清空继承的技能索引；未找到的名字一次性告警 | 4 | ✅ |
| 24 | frontmatter：`memory` | `memory.ts` | `memory.rs`：三档路径 + 信任门控 + 非法名/symlink 拒绝 + 200 行截断 + 读写/只读分支；`build_spec` 注入 append 提示并补 read/write/edit | 4 | ✅ S6d |
| 25 | frontmatter：`disallowed_tools` | `agent-types.ts` | 核心 spec `disallowed_tools`：在收窄与递归防护后强制剔除 | 4 | ✅ |
| 26 | frontmatter：`isolation` | `worktree.ts` | `AgentType.isolation`（已从 `UNSUPPORTED_FIELDS` 移出；优先级高于调用参数） | 4 | ✅ S6a |
| 27 | frontmatter：`isolated` | `agent-types.ts` | 核心 spec `isolated`：child 只保留内置工具（扩展工具全剔） | 4 | ✅ |
| 28 | frontmatter：`allowed_subagents`（嵌套**开关 + 类型白名单**） | `nested-tools.ts` | `AgentType.allowed_subagents`：`None` = 不开启嵌套；`Some([])`/`all` = 开启不限型；`Some(list)` = 开启且 `Agent` handler 按调用者 canonical 名限型（别名也解析） | 4 | ✅ S6d/S7 |
| 29 | frontmatter：`output_transcript` | `output-file.ts` | `output_file.rs`：JSONL 运行转写（事件全量落盘），路径进记录/卡片/查看器；会话子目录偏离已由 S4a 收敛 | 4 | ✅ S2/S4a |
| 30 | frontmatter 权威性（调用参数只能填空） | `invocation-config.ts` | `types.rs` + schema 构建 | 1 | ✅ |
| 31 | eject / 启用切换 / 文件生成 / 删除 / 重置（`/agents`） | `agent-file-toggle.ts` | `eject`/`enable|disable`/`delete`/`reset` 子命令 + **类型二级操作菜单** + **创建向导**（`/agents → Create new agent`，`wizard.rs`：手动写盘与 **Generate with Claude**）；**未做**：内联多行 editor 编辑现有文件（需新的核心 UI 原语） | 4 | ✅ S7c（除 Edit） |
| 32 | `<active_agent name="…">` 标签（权限系统钩子） | `prompts.ts` | `prompt.rs`：replace 前置 / append 后置 | 1 | ✅ |

### 2.2 工具与命令

| # | 特性 | 上游落点 | prux 落点 | Tier | 状态 |
|---|---|---|---|---|---|
| 33 | `Agent` 工具（`prompt`/`description`/`subagent_type` 三者必填，对齐上游 schema） | `index.ts` | `subagent.rs` | 1 | ✅ |
| 34 | `Agent` 参数：`prompt`/`description` | `index.ts` | 同上 | 1 | ✅ |
| 35 | `Agent` 参数：`name`（可寻址句柄） | `agent-manager.ts` | `manager.rs`（id 与 name 双向查找） | 1 | ✅ |
| 36 | `Agent` 参数：`model` / `thinking` / `max_turns` | `invocation-config.ts` | 同上 | 1 | ✅ |
| 37 | `Agent` 参数：`run_in_background` | `invocation-config.ts` | 同上；**默认值取自配置 `background_default`（默认后台）** | 1→2 | 🟢 见 3.1 |
| 38 | `Agent` 参数：`resume` | `agent-runner.ts` | `manager.rs` + `Session::open` | 1 | ✅ |
| 39 | `Agent` 参数：`inherit_context` | `context.ts` | spec | 1 | ✅ |
| 40 | `Agent` 参数：`isolated` / `isolation`（schema 条件出现） | `index.ts` | `isolated` 进 schema 并透传（frontmatter 优先）；`isolation`（worktree）见 S6a | 4 | ✅ |
| 41 | `get_subagent_result`（`agent_id`/`wait`/`verbose`） | `index.ts` | `subagent.rs` + `notify.rs` | 1 | ✅ |
| 42 | `steer_subagent`（`agent_id`/`message`） | `index.ts` | `subagent.rs` + `manager.rs` | 1 | ✅ |
| 43 | `Agent` 工具描述动态填充类型清单 | `index.ts` + `examples/agent-tool-description.md` | `prompt.rs`（`tools()` 时扫描生成） | 1 | ✅ |
| 44 | `toolDescriptionMode`（full/compact/custom）+ `.pi/agent-tool-description.md` | `settings.ts` | 配置 `tool_description` 三模式；custom 读 `.prux/agent-tool-description.md` > `<agent_dir()>/agent-tool-description.md`，支持占位符 `{{typeList}}`/`{{compactTypeList}}`/`{{agentDir}}`/`{{isolationGuideline}}`/`{{scheduleGuideline}}`（+ prux 早期的 `{{projectAgentsDir}}`/`{{globalAgentsDir}}`），缺失回退 full + 一次性告警 | 4 | ✅ |
| 45 | `/agents` 交互菜单（类型/运行中/调度/工作流/设置） | `index.ts` | 一级菜单 = Fleet view/子代理列表/**类型**/调度/工作流/**Create new agent**/设置；类型项现在开**交互二级菜单**（eject/enable/disable/delete/reset） | 1→2 | ✅ S7c |
| 46 | `/agents → Settings` 全量设置项 | `settings.ts` | `config.rs` **25 项** + TUI 设置面板；全局/项目**双层逐键合并**见 3.12 | 4→2 | ✅ S5/S6 |
| 47 | CLI flags（`--subagents-*`） | `index.ts` | 上游只注册了一个 flag（`--subagents-workflow-file`），已迁移（S4d，§5.12） | 4 | ✅ |
| 48 | 事件总线（`subagents:created/started/completed/failed/steered/compacted/…`） | `index.ts` | 核心 `extensions::events`（§5.17）+ 扩展发 `subagents:*` 全量生命周期事件 | 4 | ✅ S6c |
| 49 | 跨扩展 RPC（`subagents:rpc:ping/spawn/stop/consume`） | `cross-extension-rpc.ts` | `events.rs`：回执通道 `<channel>:reply:<id>`、`PROTOCOL_VERSION=2`、spawn 顶层后台+model scope、stop 仅顶层、consume 抑制通知 | 4 | ✅ S6c |

### 2.3 执行与生命周期

| # | 特性 | 上游落点 | prux 落点 | Tier | 状态 |
|---|---|---|---|---|---|
| 50 | 前台 spawn（阻塞、结果 inline） | `agent-runner.ts` | `manager.rs` + core runner | 1 | ✅ |
| 51 | 后台 spawn（返回 id、完成通知） | `agent-manager.ts` | `manager.rs` | 1 | ✅ |
| 52 | 后台并发上限 `maxConcurrent`=10 | `agent-manager.ts` | `manager.rs`（配置 `max_concurrent`，默认 10） | 1→2 | 🟢 见 3.6 |
| 53 | 前台并发上限 `maxConcurrentForeground`=0 | `agent-manager.ts` | 配置 `foreground_max_concurrent`（默认 0 = 不限，满额按 20ms 轮询排队） | 1→2 | 🟢 见 3.6 |
| 54 | 超限 FIFO 排队（`queued` 状态不占额度） | `agent-manager.ts` | `manager.rs` | 1 | ✅ |
| 55 | graceful max_turns：wrap-up 消息 + 5 grace turn + 硬 abort | `agent-runner.ts` | core `agent_session.rs` | 1 | ✅ |
| 56 | 状态集 `queued/running/completed/steered/aborted/stopped/error` | `types.ts` | `types.rs` | 1 | ✅ |
| 57 | mid-run steering（当前工具后注入） | `agent-runner.ts` | core `runtime_steer_inbox`（天然对齐） | 1 | ✅ |
| 58 | session resume（重开子会话、保留上下文） | `agent-runner.ts` | `manager.rs` + `Session::open` + `create_with` | 1 | ✅ |
| 59 | 子会话在 `/resume` 下嵌套父会话 | `agent-runner.ts` | `SessionCreateOptions.parent_session_id`（渲染已存在） | 1 | ✅ |
| 60 | `inherit_context`（fork 父会话） | `context.ts` | spec → core 复制 messages | 1 | ✅ |
| 61 | 分组 join（`smart`/`group` + 30s/15s 批量） | `group-join.ts` | `manager.rs`：配置 `join` = `smart`(30s，默认) / `group`(15s) / `async`；合并为一条 `<subagent_results>`，卡片仍逐个展示 | 2 | 🟢 见 3.5 |
| 62 | "只取消等待、不取消 agent"（Esc 取消 `wait:true`） | `abortable.ts` | `result_tool` 的 wait 与 `ctx.parent_abort` select；中止返回运行中快照、子代理继续跑 | 4 | ✅ S6d |
| 63 | 完成通知卡片（主题化 box + 可展开） | `index.ts` + UI | `CustomMessage` + `render_custom_message`：卡片=状态/用量/耗时/结果预览/session 路径（**故意只放预览**），`Continuation` 给模型全文；卡片随会话落盘并在 `/resume` 后重放 | 2 | 🟢 见 3.4 |
| 64 | 异常结局状态说明 + 部分结果抢救 | `status-note.ts` | `notify.rs` | 1 | ✅ |
| 65 | 用量累加与展示（widget/结果行/通知批量合计） | `usage.ts` | `manager.rs` 累加（含 cost）；widget 与结果行逐条展示；**合并通知外层带 batch 级 `tokens`/`duration` 合计**（`<subagent_results … tokens="N" duration="…">`） | 1/2 | ✅ S7 |
| 66 | `reportUsage`（子代理开销计入主会话） | `usage.ts` | 配置 `report_usage` + usage 池 + `AfterToolCall` 把池挂到下一个真实工具结果（核心本就计入 toolResult usage） | 4 | ✅ S6d |
| 67 | `showCost` / `showModel` | `settings.ts` | `show_cost` + `show_model`（核心 `ToolExecCtx.parent_model` → `AgentRecord.model`；继承时即父级生效型号） | 2/4 | ✅ S6d |
| 68 | nested 子代理（属主作用域委派工具、深度上限、用量上卷） | `nested-tools.ts` | `nested.rs` + `ToolExecCtx.agent_id/depth` + 设置 `max_subagent_depth`；子代理的 token 花费在收尾时累加到父记录 | 4 | ✅ S6b/S7c |
| 69 | 递归防护 + **opt-in 嵌套**：默认剔除子代理工具；只有 agent 文件写了 `allowed_subagents`（且非 `isolated`、未到深度上限）才显式放行 | —（上游同：`allowed_subagents` 开启） | `agent_types.rs` + 物化；`nested.rs` 在 `nesting_allowed` 且 `allowed_subagents.is_some()` 时把三个编排工具写进 child 白名单 | 1/4 | ✅ S7（见 3.8） |
| 70 | `.output` 流式 transcript | `output-file.ts` | 同 row 29（`output_file.rs`） | 4 | ✅ S2 |

### 2.4 上下文、环境、UI

| # | 特性 | 上游落点 | prux 落点 | Tier | 状态 |
|---|---|---|---|---|---|
| 71 | worktree 隔离 + 完成后自动提交分支 | `worktree.ts` | `worktree.rs` + `Agent({isolation})` + frontmatter `isolation` + 设置 `worktree_isolation` | 4 | ✅ S6a |
| 72 | 持久记忆（project/local/user） | `memory.ts` | 同 row 24（`memory.rs`） | 4 | ✅ S6d |
| 73 | skills 预载与筛选 | `skill-loader.ts` | `prompt::preload_skills`（具名预载/`skills: false`），见 row 23 | 1/4 | ✅ |
| 74 | model scope 白名单 | `model-scope.ts` + `enabled-models.ts` | 配置 `scope_models` + `extensions/subagent/model.rs`（复用核心 `read_enabled_models`/`resolve_model_scope`） | 4 | ✅ S5 |
| 75 | fuzzy model 解析（`haiku`/`sonnet`、`.`↔`-`、日期戳可选） | `model-resolver.ts` | `extensions/subagent/model.rs`（精确 → 打分 → 提供方回退） | 4 | ✅ S5 |
| 76 | `@handle message`（消息/续跑/重开/启动） | `mention.ts` + `mention-clone.ts` | direct 路由（`@main`/别名/steer/resume/spawn）+ `model` clone（§5.18）：隐藏回合写 prompt、顶层后台 spawn；默认模式收敛回 `model` | 4 | ✅ S6d |
| 77 | `@` 补全（名册并入文件补全） | `ui/agent-mention.ts` | 核心 `ExtensionHook::Suggestions` + `suggestion_candidates`；`suggest.rs` 把 `@handle`/`@type` 并到文件候选之前 | 4 | ✅ S6d |
| 78 | schedule（cron / `+10m` / interval / ISO） | `schedule.ts` + `schedule-store.ts` | `Agent` 的 `schedule` 参数 + `schedule/`（本地时间 cron、会话说 store） | 4 | ✅ S5c |
| 79 | 常驻 widget（all/background/off） | `ui/agent-widget.ts` | `widget.rs`：dock 多行 widget（spinner/状态色/badge/当前工具/轮数/token/成本/耗时，all·background·off 三态） | 2 | 🟢 见 3.9 |
| 80 | FleetView（编辑器下方可导航列表） | `ui/fleet-list.ts` | `fleet.rs` + 核心 Inline 覆盖层：↑↓/jk 选择、Enter 打开查看器、`s` 停止、`r` 续跑、Esc 关闭；`/agents → Fleet view` 进入 | 2 | 🟢 |
| 81 | 会话查看器（实时浮层 + 内联 steer/停止） | `ui/conversation-viewer.ts` | `fleet.rs` + 核心 Fullscreen 覆盖层：实时转写（`transcript_rows` 与 `verbose` 同口径）、当前工具行、`i`/打字聚焦内联输入、Enter = steer（运行中）/ resuming（已终结）、`s` 停止、`r` 续跑、↑↓/PgUp/PgDn/Home/End 滚动（End 复位跟随） | 2 | 🟢 |
| 82 | 查看器滚动键走用户 keybindings | `ui/viewer-keys.ts` | ✅ 滚动/翻页经 `tui.select.{up,down,pageUp,pageDown}` 解析用户 keybindings（默认仍 ↑↓/PgUp/PgDn；Home/End 保留内置语义） | 2 | ✅ S6d（§8.25） |
| 83 | 工作流卡片 / 对话框 / 菜单 | `ui/workflow-*.ts` | 卡片（S3c）+ 双栏检查器（S4c）+ `/agents workflows` 运行列表；FleetView 也纳入 live 工作流行（Enter 进检查器） | 3 | ✅ S6d |
| 84 | 调度菜单 | `ui/schedule-menu.ts` | `/agents schedules`（列表 + 取消，无向导） | 4 | ✅ S5c |

### 2.5 工作流（Tier3）

| # | 特性 | 上游落点 | Tier | 状态 |
|---|---|---|---|---|
| 85 | `SubagentWorkflow` 工具（`script`/`scriptPath`/`name`/`args`/`resumeFromRunId`） | `workflow/*` | 3 | ✅（后台化 §8.11、命名/落盘 §8.12、`resumeFromRunId` §5.10） |
| 86 | 沙箱脚本全局：`agent()`/`workflow()`/`parallel()`/`pipeline()`/`phase()`/`log()`/`args`/`budget`/`schema` | `worker-source.ts` | `workflow/glue.js`（S3，§8.9）+ `schema` 真工具注入（S3b，§8.10）+ 嵌套 `workflow()`（S7，§11.1） | 3 | ✅ |
| 87 | `meta` 块校验（纯字面量、100ms 界） | `meta.ts` | `workflow/meta.rs`（S3，§8.9） | 3 | ✅ |
| 88 | 确定性前奏（`Date.now`/`Math.random`/`eval` 抛错） | `worker-source.ts` | `workflow/bridge.rs` 前奏（S3，§8.9） | 3 | ✅ |
| 89 | 并发上限 `max(1, min(16, cpus-2))`、每运行 1000 代理、每次 4096 项 | `runtime.ts` | `workflow/bridge.rs` 配额与上限（S3，§8.9） | 3 | ✅ |
| 90 | `agent()` 的 `gate`（跑命令验证）与 `resume`（复用上下文） | `runtime.ts` | 3 | ✅（`gate` 见 §8.12，`resume` 见 §5.10） |
| 91 | 具名工作流（`.pi/workflows/` 等，`meta` 声明过滤） | `saved.ts` | 3 | ✅（S4a，§5.9；两层根 `.prux/workflows/` + `<agent_dir()>/extensions/workflows/`） |
| 92 | `resumeFromRunId` 重放 | `journal.ts` | 3 | ✅（S4b，§5.10） |
| 93 | 与外部 `Workflow` 工具的让位检测 | `collisions.ts` | 3 | ✅（S4a，§5.9；`workflows: auto\|on\|off`） |
| 94 | Claude Code `Workflow` 脚本兼容 | `worker-source.ts` | ✅ `agent/parallel/pipeline/**workflow**/phase/log/args/budget/schema`、`meta`、确定性抛错、`budget.total=null`、嵌套 `workflow()`（一层 + 256 上限）、`agent({isolation})`、未 await 启动检测均已对齐 | 3 | ✅ S7 |
| 95 | 工作流进度卡 + 双栏检查器 | `progress.ts` + `ui/workflow-*` | 3 | ✅（进度模型/卡片 §8.11；两栏检查器与 pause/skip/retry §8.14） |
| 96 | `agent({schema})` 结构化输出：注入 `StructuredOutput` 工具 + 校验 + 捕获回传 | `structured-output.ts` + `agent-runner.ts` | 3 | ✅（S3b，§5.7） |

---

## 三、行为偏离清单

> 每条格式：上游行为 / prux 行为 / 理由 / 用户动作 / 收敛时机。

### 3.1 `run_in_background` 默认后台（🟢 Tier2 已收敛）

- **上游**：`backgroundByDefault` 默认 `true`——不写 `run_in_background` 就是后台，调用立刻返回 id，
  完成时通知携带结果预览。
- **Tier1（历史）**：默认**前台**，schema 里显式写 `default: false`。理由是当时没有 widget/
  FleetView/会话查看器，后台 agent 的可观测性只有一行 dock 汇总。
- **prux（Tier2 起）**：默认值取自配置 `background_default`（**默认 `true` = 后台**，对齐上游）；
  设置面板可改回前台。schema **不写** `default`——默认值随配置变化，写死会撒谎；
  `run_in_background: false` 仍是显式前台的稳定入口。
- **用户动作**：想要 inline 结果就显式传 `run_in_background: false`；想改全局默认用
  `/agents settings`（或直接编辑 `agent_dir()/extensions/subagent.json`）。
- **回归防护**：`tests/subagent.rs::agent_tool_defaults_to_background_when_the_argument_is_omitted`
  与 `agent_tool_schema_requires_type_and_defaults_to_background`。

### 3.2 发现路径收敛为两层

- **上游**：项目 `.pi/agents/` > 项目 `.agents/agents/` > 全局 `<agentDir>/agents/`；
  同名时高优先级目录胜，同层内后加载者胜并告警。
- **prux**：`<cwd>/.prux/agents/`（受项目信任门控）> `<agent_dir()>/extensions/agent/`。
  不读 `.pi/`、不读任何 `.agents/`、不向上找祖先目录。
- **理由**：prux 的项目作用域目录是 `.prux/`（`PROJECT_SCOPE_NAME = "." + CARGO_PKG_NAME`），
  skills/SYSTEM.md 已统一在此；再支持 `.pi/` 或 `.agents/` 会形成长期双约定。
- **用户动作**：`mv .pi/agents .prux/agents`；全局 agent 放 `~/.config/prux/extensions/agent/`。
- **收敛时机**：不收敛（这是 prux 的目录约定）。

### 3.3 型号解析只做精确匹配；caller 传错即报错（✅ S5 已收敛）

- **上游**：`resolveModel` 容错（`.`↔`-` 等价、日期戳可选、同型号跨 provider 回退），
  解析不出来时 frontmatter 的 pin 退化为继承父级模型。
- **prux Tier1**：只做精确 `provider/modelId`；caller 传的 `model` 解析失败 → 硬报错；
  frontmatter 的 `model` 解析失败 → 继承父级 + 一次性告警。
- **prux S5**：`extensions/subagent/model.rs` 在扩展侧把输入解析成 canonical `provider/id`
  （精确 → 模糊打分 → 提供方回退 → 报错并列出可用型号），**核心仍然只接受精确形式**
  （`SubAgentSpec.model` 的 `split_once('/')` 不动）。两种失败都保持"报错"而不是静默继承：
  编排方明确写了型号却跑在别的型号上是最危险的失败模式。
- **仍保留的不对称**：frontmatter 的型号解析失败**不**退化为继承（上游会），仍然报错——
  配置错误不该被静默吞掉。
- **相关**：类型歧义（两个自定义只差大小写）→ **报错**而非回退（上游回退）。理由是配置错误
  不该由模型来消化。

### 3.4 完成通知 = 卡片（人）+ `Continuation`（模型）（🟢 Tier2 已收敛）

- **上游**：完成通知渲染为主题化卡片（图标 + 统计 + 结果预览），可展开看全文；走 followUp 路径
  进入对话。
- **prux Tier2**：一条后台完成投递**两件东西**，各自有明确读者：
  - **卡片**（`ExtensionUiRequest::CustomMessage` + `Extension::render_custom_message`）：
    状态图标/色、类型 badge、轮数、token、耗时、**结果预览**（12 行上限）、session 路径。
    同时 `PersistSessionEntry` 落盘，`/resume` 后由 `on_session_switched` 读会话 JSONL 重放。
  - **`Continuation`**（`<subagent_result …>` + 全文）：给模型的完整结果，不变。
- **为什么卡片只放预览**：卡片是"人一眼看完"的面，全文已经在 `Continuation` 里进了对话与
  落盘；两份全文才是重复，预览 + 全文不是。上游卡片同理默认折叠。
- **已知偏离**：重放时卡片**追加在会话末尾**，不回到原本的对话位置（要按位置回插需要把
  `customType` 条目编进消息流的渲染序，属 Tier4 打磨）。
- **用户动作**：无。`s`/`r` 等操作在 FleetView/查看器里。
- **S3c 起**：**工作流运行**的完成通知走同一条“卡片 + `Continuation`”路径，但信封是
  `<workflow_result id name status agents="done/total" tokens tool_uses duration>` +
  脚本返回值（§5.8）；卡片类型 `subagent-workflow`，`/resume` 后与代理卡片一起重放
  （`notify::cards_from_session` 按 `subagent-` 前缀认领）。

### 3.5 join 分组（🟢 Tier2 已收敛）

- **上游**：`smart`（默认，同轮 ≥2 个后台自动合并，30s 超时后部分通知 + 15s 再批）/`async`/`group`。
- **prux Tier2**：配置 `join` 三态，语义对齐：
  - `smart`（默认）：完成时若**仍有后台在跑**则攒进批次，最后一个完成即合并投递；窗口 30s 到期
    也兜底投递（定时任务带代际号，新批次使旧定时器失效）。
  - `group`：同上，窗口 15s。
  - `async`：逐个投递（上游 `async`）。
  合并文本一层信封 `<subagent_results count="N">`；**卡片仍逐个**（每张含各自预览），
  因为合并卡片会让人看不到单条结果的开头。
- **S3c 起**：批次是**中性**的（`manager::BatchItem`：代理 id 或“已渲染的信封 + 卡片”），
  所以工作流运行的完成通知与后台代理的通知**合进同一条** follow-up；外层信封名沿用
  `<subagent_results>`（它是本扩展异步工作的结果集，混着 `<workflow_result>` 项）。
- **用户动作**：`/agents settings` → *Group notifications*。
- **回归防护**：`manager.rs::join_batches_concurrent_background_completions`（两种模式各断言一次）、
  `notify.rs::batch_envelope_nests_each_result`。

### 3.6 并发额度：后台 10 硬编码、前台不设上限

- **上游**：后台 `maxConcurrent`=10；前台 `maxConcurrentForeground`=0（不限）。
- **prux Tier2**：**两项都可配置**（`max_concurrent` 默认 10、`foreground_max_concurrent`
  默认 0 = 不限）。`queued` 不占额度，完成/失败/中止立刻释放并启动队首；前台满额时按
  `PARENT_ABORT_POLL`（20ms）轮询等空位，父级回合已中止则不再等（避免中止被额度卡住）。
- **理由**：Tier1 无配置文件体系，先用硬编码；Tier2 引入 `subagent.json` 后一并收敛。
- **用户动作**：`/agents settings` → *Background limit* / *Foreground limit*。
- **回归防护**：`manager.rs::foreground_limit_serializes_foreground_runs`、
  `background_queues_when_pool_is_full_and_can_be_steered_and_stopped`。

### 3.7 `.output` transcript（Tier1 不迁移 → S2 已迁移，布局偏离已收）

- **上游**：默认给每个子代理写 `.output` 运行转写（`outputTranscript`），与 session 持久化、
  worktree 提交、memory 文件互相独立。
- **prux Tier1（历史）**：不写。子代理的可追溯性由 `persist_session` + 会话查看器承担。
- **prux S2 起**：已迁移（`subagent/output_file.rs`，row 29）：配置/`frontmatter` 双开关，
  默认 **off**；JSONL 事件全量落盘，路径进记录/卡片/查看器。
- **布局偏离（已收敛）**：S2 时路径是 `<tmp>/prux-subagents-<uid>/<encoded-cwd>/tasks/<id>.output`，
  **少一层父会话目录**（物化前拿到不父会话 id）；S4a 引入会话键缓存后已补上
  `<session>/tasks/` 那一层，与上游同形。
- **用户动作**：需要完整轨迹时开 `output_transcript`，或直接读 subagent 的 session 文件。

### 3.8 递归防护：默认剔除子代理工具，opt-in 白名单可放行

- **上游**：子代理默认拿不到 `Agent`；只有 frontmatter 写了 `allowed_subagents` 才获得
  属主作用域的委派工具（深度上限 2）。嵌套是 **opt-in** 的。
- **prux Tier1**：child 的工具白名单**无条件剔除** `Agent` / `get_subagent_result` /
  `steer_subagent`，无论类型怎么配、父级是否启用本扩展。
- **prux S6b/S7**：剔除表仍默认生效，但**显式列在 `spec.tools` 里的**保留。`build_spec` 只在
  `ty.allowed_subagents.is_some()`（开启嵌套）**且**非 `isolated`**且**未到深度上限时，
  把三个名字写进 child 白名单。用户若在 agent 文件里的 `tools:` 直接写了它们，也走这条放行，
  但 handler 的深度守卫与类型白名单仍会拦。
- **理由**：没有这层防护，child 继承父级工具集就会拿到 `Agent`，形成无上限递归与并发爆炸；
  没有 opt-in 闸门，每个子代理都会默认能再派（上游不是这个语义）。
- **用户动作**：想要子代理能委派，就在它的 agent 文件里写 `allowed_subagents: all` 或类型列表。

### 3.9 UI 与命令：Tier2 已收敛（查看器滚动键已接 keybindings）

- **上游**：常驻 widget（spinner/活动/token/状态图标）、FleetView、会话查看器、
  `/agents` 五段式菜单（类型/运行中/调度/工作流/设置）。
- **Tier1（历史）**：dock **一行**汇总；`/agents` 一个 `Select` 面板 + 三个只读子命令。
- **prux Tier2**：
  - **widget**（`widget.rs`）：标题行（running/queued/done 计数）+ 每个代理一行
    （spinner/状态色 + 类型 badge 按 `color` 着色 + id + 当前工具或任务描述 + 结局词 +
    轮数 + token + 成本 + 耗时）；`widget` 设置三态 `all`/`background`/`off`；
    运行中最多 6 行、已终结最多 3 行，避免吃满 dock 的 14 行视口。
  - **`/agents` 菜单**：一级 = Fleet view / 子代理列表 / 类型 / 调度 / 工作流 / Create new agent /
    设置（调度与工作流项按开关出现）；二级列表按 id 回查并弹结果卡片；
    `types`/`result`/`stop`/`eject`/`enable`/`disable` 子命令保留。
  - **FleetView**（`/agents → Fleet view`）：输入框下方的 Inline 覆盖层，↑↓/jk 选择、
    Enter 打开查看器、`s` 停止、`r` 续跑、Esc 关闭；选中行自动滚动到视口内。
  - **会话查看器**：全屏覆盖层，实时转写 + 当前工具 + 结果尾部；`i`/任意字符聚焦内联输入，
    Enter = steer（运行中）/ resume（已终结），`s` 停止，`r` 续跑，滚动键见下。
  - **卡片**：见 3.4。
- **滚动键（已收敛）**：查看器/列表的滚动与翻页经 `tui.select.{up,down,pageUp,pageDown}`
  解析用户 keybindings（默认仍 ↑↓/PgUp/PgDn；Home/End 仍为内置语义）。
- **理由**：只读展示避免误触发一次 LLM 调用；覆盖层把手写键盘接管收敛在一个原语里
  （`on_overlay_event`），扩展不必各自处理终端事件。
- **用户动作**：`/agents` 打开菜单 → Fleet view；或在查看器里直接操作。`/agents settings` 调设置。

### 3.10 扩展作用域：工具面已收敛、钩子面不作用域化（⚠️）

- **上游**：`tools:` 支持内置名、`*`/`all`、`none`、以及 `ext:<扩展>` / `ext:<扩展>/<工具>` 选择器。
- **prux Tier1**：支持内置名、扩展工具名、`*`/`all`、`none`；**不支持 `ext:` 选择器**
  （写了会被丢弃 + 告警）。
- **理由**：`ext:` 选择器与 `extensions:`/`exclude_extensions`（决定 child 加载哪些扩展）
  是同一套能力，Tier1 不做 extension scoping。
- **prux S2**：`tools:` 的 `ext:<扩展>` / `ext:<扩展>/<工具>` 选择器已实现
  （`expand_ext_selectors`，展开成具体工具名，未知选择器原样保留 + 一次性告警）；
  `extensions:` / `exclude_extensions:` 已实现，但作用域**只覆盖 child 的工具面**
  （模型可见/可调的工具），不覆盖扩展钩子（`transform_context` / `before_tool_call` /
  `after_tool_call` / `agent_event`）。
- **偏离理由**：prux 的钩子分发散在 agent 循环的十几处（各自遍历 `registered()`），
  要让 child 的作用域生效必须把 scope 穿过整条执行链；而钩子面作用域化的收益
  （例如让 child 不受 plan-mode 只读过滤影响）并不明确。把「scope = 模型可见的工具面」
  写成明确契约，比做一个半生效的钩子作用域更诚实。
- **用户动作**：`ext:` 选择器与 `extensions`/`exclude_extensions` 按工具面语义使用。
- **收敛时机**：不收敛（除非将来确有「child 必须与某扩展的钩子隔离」的需求）。

### 3.11 skills：继承 + 具名预载 + 可关闭（🟢 S2 已收敛）

- **上游**：`skills: true` 继承父级、`false` 不继承、逗号列表只预载指定技能。
- **prux S2**：三态齐备：
  - 缺省 / `skills: true` → 继承父级技能索引（原行为）；
  - `skills: <list>` → 用 `core::skills::load_skills` 按名字发现，正文经核心 `/skill:`
    展开器包成 `<skill name=… location=…>` 块，整体套 `<preloaded_skills>` 注入 child 提示
    （replace 模式追加到系统提示，append 模式追加到追加段）；
  - `skills: false` → 核心 spec `clear_skills`：child 不继承父级技能索引。
  找不到的技能名一次性告警（复用既有去重）。
- **用户动作**：agent 文件写 `skills: alpha, beta` 即预载；写 `skills: false` 得到干净的 child。
- **收敛时机**：已收敛。

### 3.12 设置与持久化（⚠️ 部分收敛）

- **上游**：`~/.pi/agent/subagents.json` + `<cwd>/.pi/subagents.json`，20+ 设置项，`/agents`
  设置面板可写项目级。
- **prux S5**：**两层合并**——`agent_dir()/extensions/subagent.json`（全局）→
  `<cwd>/.prux/extensions/subagent.json`（项目，**受项目信任门控**）。**逐键**覆盖：
  上层只盖它真写了的键，写坏的键回退到下层那一项（不是回退缺省）。缓存键是两层的（路径, mtime）。
  面板仍写全局层（上游面板可写项目级，见 §8.16 偏离）。
- **prux Tier2 + S2 + S5/S6/S7**：全局层 **25 项**：`widget`、`background_default`、`max_concurrent`、
  `foreground_max_concurrent`、`join`、`tool_description`、`fallback_subagent`、
  `strict_agent_files`、`disable_default_agents`、`output_transcript`、`show_tokens`、`show_cost`、
  `report_usage`、`show_model`、`agent_mentions`、`workflows`、`scope_models`、`schedule`、
  `worktree_isolation`、`max_subagent_depth`、`fleet_view`、`remember_agents`、`viewer_markdown`、
  `default_max_turns`、`grace_turns`（最后 5 项为 S7 补齐）。
  - 首次**启用扩展**时落盘默认模板（不覆盖已有文件）；面板 `apply_setting` 立即写盘。
  - 读取带缓存：`manager::config()` 用 (路径, mtime) 判缓存，widget 每帧只多一次 `stat`。
  - `viewer_markdown` 也可在查看器里按 `m` 循环（与面板写同一层）。
- **用户动作**：`/agents settings`（或编辑 `~/.config/prux/extensions/subagent.json`）。
- **收敛时机**：✅ S5（项目级合并 + 全量设置项）。

### 3.13 其它已决的 Tier1 差异（无独立条目的）

| 项 | 上游 | prux Tier1 | 收敛 |
|---|---|---|---|
| `get_subagent_result(verbose)` 上限 | 无硬上限（可展开全文） | 200 行 / 8KB 先到者，超出时保留尾部并标注 | 不收敛（沿用 Tier1 口径；结果卡片同样走这个上限） |
| Esc 取消 `wait:true` 只停等待 | 支持（`abortable.ts`） | ✅ `result_tool` 的 `wait` 与 `ctx.parent_abort` `select`；中止返回运行中快照、子代理继续跑 | ✅ S6d |
| 告警频次 | 每次触发 | 同一 (来源, 原因) 每会话一次 | 不收敛（prux 偏好去重） |
| `strictAgentFiles` | 可配置 fatal | 配置 `strict_agent_files`（默认 off = 跳过 + 告警） | ✅ S2 |
| `disableDefaultAgents` | 可配置 | 配置 `disable_default_agents`（默认 off） | ✅ S2 |
| frontmatter 的 `run_in_background` / `inherit_context` | 可把策略锁进 agent 定义 | **已锁定**：frontmatter 优先于调用参数（都未写才用全局默认） | ✅ S2 |
| `fallbackSubagent: none` | 可配置严格模式 | 配置 `fallback_subagent`（`none` 时未知/禁用类型直接报错） | ✅ S2 |
| `toolDescriptionMode` | full/compact/custom | 配置 `tool_description`；custom 模板支持 3 个占位符，未知占位符原样保留 + 告警一次 | ✅ S2 |
| `.output` transcript | 默认写（`output_transcript`） | 配置/frontmatter 双开关，默认 **off**；布局同上游（S4a 后带上会话子目录） | ✅ S2/S4a |
| `.output` 目录布局 | `<tmp>/<prefix>-<uid>/<encoded-cwd>/<sessionId>/tasks/<id>.output` | S2 曾少一层父会话目录；S4a 引入会话键缓存后已补上，**不再偏离** | ✅ S4a |
| 创建向导（`/agents → Create agent`） | 交互式逐项提问后写文件 | ✅ 手动向导 + **Generate with Claude**（`g` 交给主模型写盘）；手动路径的系统提示正文仍限单行 | ✅ S7 |
| 子代理会话的 `/resume` 根列表处理 | 父会话外的 session_dir 单独列出 | 沿用 prux 既有行为 | — |
| `/agents` 子命令 | 五段式菜单（类型/运行中/调度/工作流/设置） | ✅ 一级菜单（Fleet view / 列表 / 类型 / 调度 / 工作流 / Create new agent / 设置）+ `types`/`result`/`stop`/`eject`/`enable`/`disable` | ✅ S6 |
| frontmatter 的 `run_in_background` / `inherit_context` | 可把策略锁进 agent 定义 | **已锁定**：frontmatter 优先于调用参数（都未写才用全局默认） | ✅ S2 |
| 前台失败回报 | 工具结果为 `status=error` 的普通结果 | **同上游**：物化/执行失败以 `status=error` 的普通结果回报，只有请求本身不合法才是 tool error | 不收敛 |

---

## 四、用户迁移动作

把现有的 pi-subagents 用法搬到 prux，需要做的事**只有**下面 5 条：

1. **移动 agent 定义文件**
   ```bash
   mkdir -p .prux/agents && mv .pi/agents/*.md .prux/agents/   # 项目级
   mkdir -p ~/.config/prux/extensions/agent && mv ~/.pi/agent/agents/*.md ~/.config/prux/extensions/agent/  # 全局
   ```
   删除 `.agents/agents/` 的用法（不再读取）。

2. **改工具名**：`task` → `Agent`。旧 `task` 工具已删除，**没有别名**。

3. **默认前后台与上游一致（Tier2 起）**：不写参数 = 后台。
   - 想要 inline 结果：显式 `run_in_background: false`。
   - 想要前台并发：在**同一条消息里**发多个 `run_in_background: false` 的 `Agent` 调用。
   - 结果取回：前台直接 inline；后台等 `Continuation` 通知，或用
     `get_subagent_result(agent_id, wait: true)`。
   - 全局改回默认前台：`/agents settings` → *Background by default* = `off`
     （旧 Tier1 行为，可作逃生舱）。

4. **检查 agent 文件里 Tier1 会忽略的字段**：
   `extensions` / `exclude_extensions` / `skills` / `memory` / `disallowed_tools` /
   `isolation` / `isolated` / `allowed_subagents` / `output_transcript` /
   `session_dir`（`session_dir` 支持，但只在 `persist_session: true` 时有意义）、
   `tools:` 里的 `ext:` 选择器。
   忽略会告警一次，不会失败。**frontmatter 的 `model:` 必须写完整 `provider/modelId`**
   （模糊简写不支持，解析失败退化为继承父级）。

5. **`prompt_mode: replace` 的语义收紧**：prux 的 replace 会**清空 `context_files`**，
   即不注入 `.prux/SYSTEM.md` / `AGENTS.md`。需要项目约定的 agent 用 `append`。

**明确不需要做的事**：不需要手工创建 `subagents.json`（首次启用扩展会落盘默认模板）；
不需要改 `.output` 转写配置（不写）；不需要为 `/resume` 做任何准备（子会话自动嵌套）。

---

## 五、核心改动清单（契约冻结）

> 本节是 `SubAgentSpec` / `SubAgentControls` / `SubAgentRunner` / `ToolExecCtx` /
> `SessionCreateOptions` 的**唯一权威版本**。扩展与核心两边都对着这里写。

### 5.1 `src/core/extensions/tools.rs`

```rust
pub type RunSubAgentFnRet   = BoxFuture<'static, std::result::Result<String, String>>;
pub type RunSubAgentFn      = dyn Fn(String) -> RunSubAgentFnRet + Send + Sync;   // ← Tier1 删除
pub type SpawnSubAgentFnRet = JoinHandle<std::result::Result<String, String>>;
pub type SpawnSubAgentFn    = dyn Fn(String) -> SpawnSubAgentFnRet + Send + Sync; // ← Tier1 删除

/// 一次子代理物化的完整描述。核心只解释字段，不做任何策略判断。
#[derive(Clone)]
pub struct SubAgentSpec {
    /// 类型名（诊断与展示；核心不解释，扩展用于通知/记录）
    pub agent_type: String,
    /// prompt_mode = replace：作为完整系统提示（同时清空 context_files）
    pub system_prompt: Option<String>,
    /// prompt_mode = append：追加到父级系统提示之后
    pub append_system_prompt: Option<String>,
    /// replace 模式下是否清空父级 context_files（AGENTS.md / .prux/SYSTEM.md）
    pub clear_context_files: bool,
    /// 精确 `provider/modelId`；None = 继承父级
    pub model: Option<String>,
    /// thinking 级别；None = 继承父级。不被支持时由核心钳制
    pub thinking: Option<String>,
    /// 工具白名单（内置名 + 扩展工具名 + `*`/`all`/`none` 已由扩展解析完毕）
    pub tools: Option<Vec<String>>,
    /// 0 或 None = 无上限
    pub max_turns: Option<u32>,
    /// 把父级当前对话复制进 child
    pub inherit_context: bool,
    /// 覆盖 cwd；None = 继承父级（Tier4 worktree 用）
    pub cwd: Option<String>,
    /// 是否落盘子会话（决定 SessionCreateOptions.persist）
    pub persist_session: bool,
    /// 续跑：打开既有子会话并以其上下文继续（None = 新建）
    pub resume_session_path: Option<String>,
    /// 显式覆盖父会话 id；None = 由核心自动取父 agent 的 session id
    pub session_parent_id: Option<String>,
    /// 覆盖子会话目录
    pub session_dir: Option<std::path::PathBuf>,
    /// 可寻址句柄（诊断/展示；核心不解释）
    pub name: Option<String>,
    /// 每 child 独立事件流：核心把 child 的 json_sink 指向它
    pub events: Option<Arc<Mutex<crate::core::agent_session::JsonSink>>>,
}

/// 运行期控制句柄：`run()` 独占 runner 时仍可从任意线程/任务持有。
///
/// **不携带 id**（见 5.4 修正 1）：排队中的 agent 尚无 runner，而 id 必须在入队时就存在，
/// 因此 id 由扩展分配，核心不知道也不需要知道。
pub struct SubAgentControls {
    /// 注入 steer（内部写入 child 的 runtime_steer_inbox）
    pub steer: Arc<dyn Fn(String) + Send + Sync>,
    /// 置位即中止 child（与父级 abort 联动由核心在物化时决定）
    pub abort: Arc<std::sync::atomic::AtomicBool>,
    /// 子会话落盘路径（persist_session = true 时 Some）
    pub session_path: Option<String>,
}

/// 扩展拥有它并自行 spawn / await。
pub trait SubAgentRunner: Send {
    fn controls(&self) -> SubAgentControls;
    /// 驱动一次 prompt。注意：`run` 只跑一轮 prompt（含其内部多 turn），
    /// resume 由扩展重新物化 + 回灌 messages 完成。
    fn run(&mut self, prompt: String) -> BoxFuture<'_, std::result::Result<String, String>>;
}

pub type MakeSubAgentFn = dyn Fn(SubAgentSpec) -> std::result::Result<Box<dyn SubAgentRunner>, String>
    + Send + Sync;

#[derive(Clone)]
pub struct ToolExecCtx {
    pub cwd: String,
    /// 新增：按 spec 物化隔离 child
    pub make_sub_agent: Arc<MakeSubAgentFn>,
    /// 新增：父级当前回合的取消信号（前台 child 联动用）
    pub parent_abort: Arc<std::sync::atomic::AtomicBool>,
}
```

### 5.2 `src/core/agent_session.rs`

物化职责（新增私有函数，`make_exec_ctx` 改为构造 `make_sub_agent` 闭包）：

1. 从 `AgentTemplate` 克隆；按 spec 覆盖：
   - `model` → `model_resolver::find_model` + `model_config_from_entry`
   - `thinking` → `set_thinking_level_silent`（钳制）
   - `system_prompt` / `append_system_prompt` / `clear_context_files` → 写 `rebuild_ctx` 后
     `rebuild_tools()`
   - `tools` → `set_active_tools(names)`（`*`/`all` → 父级全集；`none` → 空）
   - `inherit_context` → `messages = 父级 messages.clone()`
   - `persist_session` → `Session::create_with(SessionCreateOptions { cwd, session_dir,
     persist: true, parent_session_id: spec.session_parent_id.or(父级 session id), ..Default })`
   - `events` → `attach_json_sink`
2. **递归防护**：无论 spec 怎么写，最终 `set_active_tools` 前剔除
   `Agent` / `get_subagent_result` / `steer_subagent`。
3. **max_turns 优雅收尾**：`finish_turn`（pi 0.87.0 前为 `should_stop_after_turn`）数 turn，
   到 `max_turns` 时经 steering 队列注入 wrap-up 消息；`grace_turns`（Tier1 硬编码 5）后返回
   `FinishTurnAction::End`。结局状态由扩展从事件推断（`steered` vs `aborted`）。
4. **前台 abort 联动**：`make_sub_agent` 无法知道调用方是谁，故由**扩展**在 spec 之外
   决定是否联动 —— 见下条。

> **实现注记（已落地）**：`ToolExecCtx.parent_abort` 是父级回合的取消信号。扩展对**前台** spawn
> 执行联动：`futures_util::future::select` 在 runner 的 `run()` 与「轮询 `parent_abort`」之间
> 竞速（20ms 间隔），父级中止时置位 `controls.abort` 后继续 await 子代理收尾
> （保留部分输出）。后台 spawn 不联动。之所以放在扩展而不是核心：核心的 `make_sub_agent`
> 不知道这次 spawn 是前台还是后台。
>
> 工具白名单用 `ToolSelection::Allowlist` 而非 `set_active_tools` 的 `Default`：
> `Default` 会把扩展工具**无条件**并入，无法表达收窄；`Allowlist` 对内置与扩展工具同时生效。

### 5.3 `src/core/session_manager.rs`

```rust
#[derive(Debug, Clone, Default)]
pub struct SessionCreateOptions {
    pub cwd: String,
    pub session_dir: Option<PathBuf>,
    pub persist: bool,
    /// None → 生成新 v7 UUID
    pub session_id: Option<String>,
    /// None → header 不写 parentSessionId
    pub parent_session_id: Option<String>,
}

impl Session {
    /// 唯一构造入口（现 `create_with_id` 主体迁移至此）
    pub fn create_with(opts: SessionCreateOptions) -> Result<Session>;

    // 薄包装，既有 42 个调用点零改动
    pub fn create(cwd: &str, session_dir: Option<PathBuf>, persist: bool) -> Result<Session>;
    pub fn create_with_id(cwd: &str, session_dir: Option<PathBuf>, persist: bool, session_id: &str)
        -> Result<Session>;
}
```

配套新增只读取值器（供测试与扩展读取父会话）：

```rust
impl Session {
    /// 父会话 id（fork / 子代理会话；`/resume` 嵌套显示依赖它）
    pub fn parent_session_id(&self) -> Option<&str>;
}
```

要点：`#[derive(Default)]` 让未来加字段可走 `..Default::default()` 的加性扩展路径；
`/resume` 选择器对 `parent_session_id` 的嵌套渲染**已存在**（`session_selector.rs:758`），
无需改动。

### 5.4 实施中的契约修正（4 处，均为实施时发现的真实约束）

冻结契约在落地时被现实修正了 4 处。每处都给出理由，避免后来者当成笔误改回去。

1. **`SubAgentControls` 去掉 `id` 字段。** 冻结稿写"核心分配稳定 id"，但 FIFO 排队意味着
   agent 在**还没有 runner** 时就必须有 id（排队中的 agent 要能被 `steer_subagent` /
   `get_subagent_result` / `/agents stop` 按 id 寻址）。因此 id 由扩展分配，核心不知道也不需要知道。
   `SubAgentSpec.name` 保留为纯展示字段。
2. **`SubAgentSpec` 新增 `resume_session_path`。** 冻结稿没有"从既有会话续跑"的入口：
   `inherit_context` 复制的是**父级**对话，无法表达"打开某个子会话并继续"。核心在该字段存在时
   改为 `Session::open(path)` + `build_context_messages()` 回灌，且**不再新建**会话文件
   （消息继续追加到原文件）。
3. **第 4 处核心改动：`core/project_trust.rs` 的 `PRUX_RESOURCES` 增加 `"agents"`。**
   该常量决定"项目里有哪些资源会触发信任询问"。不加这一项，只含 `.prux/agents/` 的项目
   永远不会被询问信任，于是 `is_project_trusted()` 恒为 false、agent 定义恒被忽略，
   且用户没有任何途径把它打开——功能等于不可用。
4. **失败结局用 `error` 而不是 `failed`。** 上游 `types.ts` 的 `AgentStatus` 联合类型是
   `queued | running | completed | steered | aborted | stopped | error`；冻结稿漏了 `error`。

另外一处**实施期的能力补强**（不是修正，但契约里没有）：`Extension::tools()` 没有入参，
无法得知会话 cwd，而项目级 `.prux/agents/` 必须按会话 cwd 发现。实现改为在
`on_session_start(cwd, …)` 记下会话 cwd 供工具描述使用（spawn 时仍一律用 `ToolExecCtx.cwd`
重新发现，因此这里滞后只影响描述里的类型清单，不会派发到错误类型）。

### 5.5 Tier2 新增的核心原语（契约冻结）

Tier2 只往核心加了**两处**原语，都为"扩展自有的交互面"服务，不引入子代理专属概念：

```rust
// src/core/extensions/overlay.rs（新模块）
pub enum OverlaySize { Inline { max_rows: u16 }, Fullscreen }
pub struct OverlayInput { pub label: String, pub value: String, pub placeholder: String }
pub struct OverlayView {
    pub title: String,
    pub lines: Vec<DockLine>,          // 复用 dock 的带样式行
    pub footer: Vec<(String, String)>, // 键位提示
    pub input: Option<OverlayInput>,   // 底部内联输入行
    pub size: OverlaySize,
    pub follow: bool,                  // 贴底跟随（查看器）
    pub selected: Option<usize>,       // 选中行（列表类；TUI 保证它可见）
    pub input_focus: bool,             // 输入聚焦请求（只在变化沿生效）
}
pub enum OverlayKey { Up, Down, PageUp, PageDown, Home, End, Enter, Esc, Backspace, Tab, Char(char), Other }
pub enum OverlayEvent { Key { key: OverlayKey, input: Option<String> }, Submit { text: String }, Closed }

pub fn overlay_view(id: u64) -> Option<OverlayView>;   // 每帧拉取（Hook::Overlay 门控）
pub fn overlay_event(id: u64, ev: OverlayEvent) -> bool; // 回传；true = 扩展已消费

// Extension trait 新增
fn overlay_view(&self, id: u64) -> Option<OverlayView> { None }
fn on_overlay_event(&self, id: u64, ev: &OverlayEvent) -> bool { false }
fn render_custom_message(&self, custom_type: &str, data: &Value) -> Option<Vec<RichSpan>> { None }

// ExtensionUiRequest 新增变体
ShowOverlay { id } / HideOverlay { id }
CustomMessage { label, custom_type, data }

// src/core/extensions/ui.rs
pub fn render_custom_message(custom_type: &str, data: &Value) -> Option<Vec<RichSpan>>;
```

约定（实现覆盖层/卡片的扩展必须遵守）：

1. `overlay_view` / `on_overlay_event` / `render_custom_message` 都在 **TUI 线程**调用，
   **不得加 agent 锁**（与 `/agents` 的 `busy_safe = true` 同一约束）。
2. `render_custom_message` 必须**纯函数**：同一份 `data` 会在实时投递与会话重放两处调用。
3. TUI 默认语义（扩展返回 `false` 时）：Esc 关闭、↑↓/PgUp/PgDn/Home/End 滚动、
   有输入行时字符/退格编辑、`i`/任意字符聚焦输入、Enter 提交。
4. `input_focus` 只在**变化沿**生效，用户"打字即聚焦"不会被每帧重置。
5. `DockSpawn::fallback` 放宽为 `Cow<'static, str>`（Tier2 唯一的核心类型放宽）：
   动态 badge 色走 `DockSpan::hex`（`key` 空 + `fallback` 为 `#RRGGBB`）。

### 5.6 S2 收尾的核心接缝（契约冻结：`@mention`）

> 经 10 轮拷问定案（本切片按此实现，S6 再补 `model`/clone 与 tombstone）。

**A. 用户输入拦截钩子（统一现有 `on_user_input`）**

```rust
// src/core/extensions/hooks.rs
pub enum ExtensionHook {
    // …既有…
    /// 用户 Enter 提交前的输入拦截（含忙碌时的 steer 提交）。
    UserPrompt,
}

// src/core/extensions.rs
pub enum UserPromptAction {
    /// 不认领，继续走原提交路径（可携带改写后的文本）。
    Continue,
    /// 用改写后的文本继续原提交路径。
    Rewrite(String),
    /// 认领：不启动回合、不排队（扩展自行投递）。
    Handled,
}

// Extension trait：`on_user_input` 改名为 `on_user_prompt` 并返回动作枚举。
fn on_user_prompt(&self, text: &str) -> Option<UserPromptAction> { None }

/// 分发：按注册顺序，首个返回 `Some` 的**已声明 UserPrompt** 扩展生效。
pub fn dispatch_user_prompt(text: &str) -> Option<UserPromptAction>;
```

- **门控**：`hooks()` 含 `UserPrompt` 才被调用（动态兴趣，对齐 `Dock`/`Overlay`）。
- **触发点**：空闲提交（`handle_input_submit_key` 的 plain-text 分支、两次
  skill/template 展开**之后**，原 `apply_extension_input_transform` 位置）与**忙碌提交**
  （`handle_busy_key` 在 `queue_steer` 之前）。`Handled` → 两者都不启动回合/不排队；
  `Rewrite` → 用改写文本继续。
- **迁移**：`plan_mode` 声明 `UserPrompt` 并返回 `Rewrite`（原语义不变）；`goal` 返回 `Continue`。

**B. 会话执行上下文的钩子（让提交时也能 spawn/resume）**

```rust
/// 核心在会话建立/回合边界把当前 agent 的 `ToolExecCtx` 交给扩展；
/// 扩展缓存它（Tier4 用 `fleet::remember_ctx`），使 `on_user_prompt` 里也能
/// 拿到 `make_sub_agent`（原先只有工具执行期间才有）。
fn on_exec_ctx(&self, _ctx: &ToolExecCtx) {}

/// 分发（不分高低频，与 on_session_start 同性质，不加门控）。
pub fn dispatch_exec_ctx(ctx: &ToolExecCtx);
```

刷新点：`src/main.rs` 构建完 agent 后一次（覆盖首条消息）、`core/agent_session.rs`
每次 turn 开始（模板保鲜）、`modes/interactive/worker.rs` 会话切换后（避免 `/resume`
到别的项目后仍拿旧 cwd 的模板）。

**C. 扩展侧：`@mention` 路由（照上游 `mention.ts` + `index.ts` 的 1–5、7 分支）**

新增 `src/extensions/subagent/mention.rs`：`handle_base` / `assign_handle` /
`parse_mention`（`^@([\w-]+)\s+([\s\S]+)$`，trim 非空）/ `strip_agent_prefix` /
`describe_mention` / `resolve_handle_to_type`；`main` 保留、`MAX_HANDLE_LENGTH = 64`、
大小写不敏感。

`AgentRecord` 增两个字段：`handle: Option<String>`（类型名派生）与
`alias: Option<String>`（`req.name` 经 `handle_base` + `assign_handle`，仅给了 name 时）。
两者**共用一个命名空间**（分配时同时避让），spawn 时分配；**淘汰即回池**（只扫存活记录，
不做 tombstone）；**resume 保留原 handle**。

路由（`subagent` 的 `on_user_prompt`）：

| 分支 | 输入 | 行为 |
|---|---|---|
| `@main <text>` | 保留句柄（大小写不敏感） | `Rewrite(text.trim())` |
| `@agent-<x> …` | 直接解析失败时按 `<x>` 再解析一次 | 同下 |
| 存活且 running/queued | 句柄或 alias 命中 | steer + `Sent to @x`，`Handled` |
| 存活且已终结、有 session | 同上 | 后台 resume（复用同 id/handle）+ `Resuming @x`，`Handled` |
| 存活且已终结、无 session | 同上 | 落到类型派发 |
| 类型（从未跑过） | `resolve_handle_to_type` 命中 | 后台 spawn（`description = describe_mention`）+ `Started @x`，`Handled` |
| 裸句柄 / 未知句柄 / 解析不到类型 | — | `Continue`（不吞输入，落回主模型） |

**D. 设置**：`config.rs` 增 `agent_mentions: off | direct | model`，**默认 `direct`**
（S6d 已按 §5.18 收敛回 `model`）；S2 当时 `model` 降级为 `direct` 并一次性告警
（clone 未实现，S6 已补，该降级告警已移除）；`off` 由 `on_user_prompt`
第一行返回 `None`（`hooks()` 常声明 `UserPrompt`，因为该函数在流式事件里高频调用，
不宜在其中做 `manager::config()` 的 `stat`）。

**E. 可发现性（弹窗推后）**：`/agents` 列表行与 widget/结果卡显示 `@handle`；交互式 `@`
句柄候选弹窗留给 S4/S6（`ui/agent-mention.ts`）。

**F. 明确不做（本切片）**：tombstone 重开（分支 6）、`model`/mention-clone、`@` 候选弹窗。

### 5.7 S3b 的核心接缝（契约冻结：per-child 工具注入）

**背景**：上游 `agent({schema})` 给子代理注入一个 `StructuredOutput` 工具（`structured-output.ts`）——
工具名 / 描述 / snippet / guidelines / `parameters`（= 调用方 schema）/ `prepareArguments` 全在扩展侧。
prux 的 `SubAgentSpec.tools` 只能**收窄**已有工具，无法注入新工具，所以 S3 曾以
"提示词要求 JSON + 宽松解析"近似（§8.9）。S3b 补上真注入口，形态取"核心只做机制"：

```rust
/// 注入到某个 child 工具表的工具。元数据复用 ExtensionTool（其字段与上游 defineTool 的
/// 入参一一对应：name/description/snippet/prompt_guidelines/parameters/constrained_sampling/
/// prepare_arguments），只多一个 handler。
pub struct InjectedChildTool {
    pub meta: ExtensionTool,
    /// 参数已由核心 `validate_tool_arguments` 校验过；返回 Err 即 isError 工具结果。
    /// 同步：注入工具的语义是"记录/短路"，不驱动别的执行；也不给 `ToolExecCtx`
    /// （它不是扩展工具，与 cwd/子代理调度无关）。
    pub handler: Arc<dyn Fn(Value) -> Result<ToolResult, ToolError> + Send + Sync>,
}

// SubAgentSpec 唯一新增字段
pub injected_tools: Vec<InjectedChildTool>,
```

**落点与不变量**（`core/agent_session.rs`）：

- `ToolRebuildCtx.injected_tools: Arc<Vec<InjectedChildTool>>`——`snapshot_template` 会 clone 它，
  因此**并行与顺序两条工具执行路径**都能拿到（`execute_one` 只持有 `Arc<AgentTemplate>`，没有 `Agent`）。
- `compose_tools` 在 selection **之后无条件并入**注入工具（含 snippet/guidelines），对齐上游
  `readmitToolNames`"在每个闸门一起放回"，并把注入名推进 `selected`——系统提示的
  Available tools 段是按 `selected_tools` 查 snippet 渲染的，不推进去子代理就看得到工具名但
  看不到它是干什么的。推论：`tools:` 白名单 / `disallowed_tools` / `isolated` /
  `extensions:` / `exclude_extensions` **都摘不掉**注入工具（它们只作用于 selection）。
- **不进 `get_all_tools()`**：它是 `materialize_sub_agent` 算"父级宇宙"用的；注入工具是 per-child 的，
  既不能被 `tools:` 选中，也不能被孙代理继承。
- **同名冲突**：注入赢，并 `note_warning` 一次（不报错——第三方扩展取了这个名字不该让所有 schema
  工作流全盘失败，但也不能静默）。
- **不加**进 `SUBAGENT_TOOL_NAMES`：那是递归防护的剔除表，注入发生在剔除之后，加进去只会误导。

**工具解析收敛为 `ToolIndex`**：核心原有 6 处"按名字查工具"（`is_builtin_tool` /
`extension_has_tool` / `apply_prepare_arguments_any` / `extension_tool_params` /
`tool_execution_mode_any` / 派发遍历 `registered()`）全是"按名字查全局注册表"，注入工具在每一处
都查不到。S3b 收成 `ToolIndex`（`exists` / `prepare_arguments` / `parameters` / `execution_mode` /
`is_builtin`），优先级**注入 > 内置 > 扩展注册表**写在一处；`prepare_tool_calls` 是关联函数
（无 `&self`），以参数取用。

**已知缺口（有意，见 §8.10）**：provider 层 `tool_has_constrained_sampling`
（`provider/completions.rs:396`）只收到 `(name, description, schema)` 三元组，按名字查内置与
全局扩展工具，**看不到** per-child 注入，所以注入工具拿不到 provider 侧 strict 约束采样
（上游 `constrainedSampling: { strict: "prefer" }` 三重压力里的第 1 重）。

**kernel 侧的 schema 可行性闸门**（`workflow/bridge.rs`）：

```rust
// WorkflowInputs 新增
pub validate_schema: Option<Arc<dyn Fn(&Value) -> Result<(), String> + Send + Sync>>,
```

在 dispatch 里 `parse_agent_request` 之后、spawn **之前**调用；`Err` → `settle(ok:false,
{message, fatal:true})` 并 `continue`。位置与上游 `runtime.ts:832` 逐字对应（编译在 `agentCount++`
与并发 permit 之前），故失败的调用不占并发额度、不计入 1000 代理配额。钩子返回 `Result<(), String>`
而非编译产物 → **kernel 侧不引入 `CompiledSchema` 类型**；schema 因此编译两次（bridge 一次判致命、
`run_one_agent` 一次建工具并缓存给每次 payload 校验），两次调同一个 `structured_output::compile`。

**agent 失败 = `null`（修正既有偏差，见 §8.5）**：kernel settle 语义改为——普通 agent 失败
（error / stopped / max_turns 用尽）→ `ok:true, "null"`，`agent()` **返回 `null`**；
致命（配额超限 / schema 编译失败 / 未知 host 方法）→ `ok:false + fatal:true`。
对齐上游 `runtime.ts:1124` + `worker-source.ts:533`，并修正 prux 原先"工具描述承诺返回 null、
实现却 reject"的自相矛盾。`AgentOutcome` / `AgentRunner` 契约不变。

### 5.8 S3c 的核心接缝（契约冻结：工作流后台化 + 实时进度）

**后台契约**（对齐上游）：`SubagentWorkflow` **立刻**返回，正文是
`Workflow "<name>" started in the background.\nTask ID: <wf_…>\n\nYou will be notified when it finishes — do NOT poll or sleep waiting for it.`，
`details = { taskId, workflow, status: "running" }`；运行在工具返回之后继续，完成时经
**join 批次**发一条 `Continuation`（模型）+ 一张 `CustomMessage` 卡片（人）。
脚本解析与 `meta` 校验在登记**之前**：写错的脚本是工具错误，不会留下一个只能等通知的任务。

**运行注册表**（`workflow/task.rs`，上游 `task.ts` 的落点）与 `manager` **并列**：
`AgentRecord` 的字段（轮数/工具次数/steer/转写）对一次运行大半无意义，反之 run 的字段
（返回值/进度日志）也不属于一个 agent。两个注册表在**扩展边界**组合：

| 组合点 | 形式 |
|---|---|
| `Extension::dock_lines()` | `manager::widget_lines() ++ task::dock_lines()`（两段各有 header；dock 只列**运行中**的 run） |
| 运行列表覆盖层 | `/agents workflows` → `fleet::open_workflows()`；内容来自 `task::overlay_view()`，**只读**（不消费任何键，滚动走 TUI 默认） |
| 完成通知 | `manager::BatchItem`（见下） |
| `reset_all` | 会话切换 / 扩展禁用时两者一起清空 |

**取消语义**：run 持有**自己的** `Arc<AtomicBool>`，并以它派生一个 `ToolExecCtx` 去派发子代理
（“死 run 就死它的全部子代理”）。**启动回合的 `parent_abort` 与它无关**——工具已经返回、模型已经
往下走，那个标志只会被同回合/后续回合的 Esc 触动，绑上去会让一个后台 fan-out 的子代理被无端掐死、
而脚本带着一串 `null` 跑到终点交回一个假结果。停止只能来自：`/agents stop wf_…`、
`reset_all`（会话切换 / 扩展禁用）。

**进度日志归 run 所有**：`WorkflowInputs.progress: task::ProgressLog`（`Arc<Mutex<Vec<Value>>>`），
由 run 创建并传入，JS 的 `__progress` 与宿主（kernel）都往里追加；`progress_sink` 已删除
（两个界面都是每帧拉取，没有推送消费者）。`WorkflowOutcome` 里去掉了 `progress`（不再复制一份）。

**进度条目由 kernel 写入**（上游同样在 host/runtime 侧 emit，因为并发信号量、agent index、结果都在那一侧）：
每次 `agent()` 三次跃迁，每次**改同一个 `Value` 再整体重发**（折叠按 `index` last-write-wins，
所以结算那条必须带着前面累积的字段）：

| 时机 | 字段 |
|---|---|
| 派发（spawn 之前） | `type/index/label/state:"start"/agentId:"wf-agent-N"/agentType/prompt/promptPreview/queuedAt` + `phaseIndex/phaseTitle`（来自 JS）+ `model`（脚本**请求**的） |
| 拿到并发 permit | `state:"progress"`, `startedAt`, `lastProgressAt` |
| runner 返回 | `state:"done"|"error"`, `lastProgressAt`, `durationMs`, `recordId`, `tokens`, `toolCalls`, `skipped`（被停掉的子代理）；失败时 `error` |

`index` 与配额计数器是**同一个数**（上游 `const index = agentCount++`），配额检查在自增**之前**
（被拒的调用不消耗计数器），且 schema 闸门在计数器之前。`budget.spent()` 由结算回报的
**output** token 累加（上游口径：把 input/cache 也算进去会高估一个数量级）。

**kernel 的类型变化**：`AgentOutcome` 不再直接返回，而是装在 `AgentReport { result: Result<AgentOutcome, String>, meta: AgentMeta }` 里——
结局用**内层** `Result`，这样失败也带元数据（被停掉的子代理要标 `skipped`，只有 runner 知道）。

**通知批次变中性**（`manager::BatchItem`）：

```rust
pub(crate) enum BatchItem {
    Agent(String),
    /// 已经渲染好的（continuation 信封 + 可选卡片）——批次只需知道“有一条通知以及它的正文”，
    /// 不需要知道它是什么，于是 `workflow::task` 能复用同一套 join 窗口/代际而不倒挂依赖。
    Rendered { envelope: String, card: Option<(String, serde_json::Value)> },
}
```

`flush_batch` 改为通用路径（1 条 → 原样；多条 → `notify::batch_envelope` 套一层）；
`notify::batch_envelope` 接受 `&[String]`（已渲染的信封）而不是 `&[AgentRecord]`。
run 的信封是 `<workflow_result id name status agents="done/total" tokens tool_uses duration>` +
脚本返回值（超 4000 字符截断），失败/被停时正文是错误文本。

**计数口径对齐上游**：头部一律 `done/total`（失败**不计入分子**），失败靠状态后缀
（`· failed` / `· stopped`）与卡片里的失败行体现。

**`Extension::wants_redraw()`**（新钩子，默认 `false`）：TUI 只在 `dirty || busy` 时重绘，
所以后台活儿在跑、界面 idle 时 dock 是冻的（Tier2 的后台代理 spinner 以前就有这个毛病）。
实现返回 `true` 会让那 80ms 的动画 tick 持续置脏；`subagent` 返回
`manager::background_live() > 0 || task::has_live()`。覆盖层打开时 TUI 本就每帧重绘（
`overlay::sync` 结尾无条件置脏），不依赖它。

### 5.9 S4a 的核心接缝（契约冻结：gate / 具名工作流 / 脚本落盘 / collisions）

四项都不需要新的核心接缝，全部落在扩展内：

**`agent({ gate })`**（上游 `host.ts` 的 `executeGate` + runtime 的 `applyGate`）
- JS 选项表加入 `gate`（不再报"未实现"）；`AgentRequest.gate` 带上来。
- 语义：子代理**成功之后**跑 `<gate>`（`sh -c` / `cmd /c`，cwd = 该工具调用的 cwd），
  非零退出即该 agent 失败、**命令输出就是错误** → 脚本侧 `agent()` 得 `null`；
  失败/被停的子代理**不跑** gate（它的失败已经说明问题）。
- 超时 10 分钟（上游 `DEFAULT_GATE_TIMEOUT_MS`），超时**明确**按失败并说明超时
  （上游踩过的坑：被杀但退出码 0，只看退出码会把被杀的 gate 读成通过）。
- 位置：`run_one_agent` 里跑，而它是在桥的并发 permit 持有期间被调的——gate 因此占着
  它那个槽位（上游注释的原话：一个挂死的 gate 会一直楔住那个槽位），这正是必须有超时的原因。

**具名工作流**（`workflow/saved.rs`，上游 `saved.ts`）
- 两层根（§3.2 约定，与 agent 发现同构）：`<cwd>/.prux/workflows/<name>.js`（**项目信任门控**
  后）→ `<agent_dir()>/extensions/workflows/<name>.js`，**first-hit-wins**（项目优先）。
- 名字先校验再拼路径：`^[a-zA-Z0-9][a-zA-Z0-9._-]*$`、≤128；**显式拒绝符号链接**
  （根与文件两处）——agent 发现是枚举目录，这里是"模型给的名字 → 拼路径"，
  正是不该跟着链接走的那一步。
- 只有带 `export const meta =` 声明的 `.js` 才算工作流（纯正则、从不执行）。
- **优先级修正**：`scriptPath` > `script` > `name`（上游描述里写着 `scriptPath`
  "Takes precedence over `script`"，而 prux 此前实现反了）；`name` 命中时把该文件同时记为运行的
  `scriptPath`，于是"编辑它再跑"对具名工作流同样成立。
- 失败消息带上搜过的根与实际存在的名字（猜错名字的模型可自纠）。

**脚本自动落盘**（上游 `index.ts` 的 `sessionTaskDir` + `<runId>.workflow.js`）
- 每次调用把脚本体写进**本会话的任务目录**，正文返回 `Script: <path>` 与
  "To iterate, edit the script file and call SubagentWorkflow again with scriptPath"。
- 目录形状与上游一致：`<tmp>/prux-subagents-<uid>/<encoded-cwd>/<session>/tasks/`，
  其中 `<session>` 是**会话文件的 stem**（[`session`] 缓存：`on_session_start` 给 `Session`、
  `on_session_switched` 给路径）。没有落盘文件的会话退化为不带会话层的老形状。
  —— 这同时**收掉了 `output_file.rs` 里那条"拿不到父会话 id"的已知偏离**
  （`.output` 转写现在也按会话分子目录）。
- 落盘失败只是少一个便利（去掉 `Script:` 行），不影响运行。

**`collisions` 让位**（上游 `collisions.ts`）
- 三态配置键 `workflows: auto | on | off`（默认 `auto`），序列化上游的
  "默认让位于证据，选择不让位"——prux 的配置没有"用户显式写过这个键"的概念，所以把
  三种行为直接变成三态而不是 `bool + pinned`。
- 冲突定义：**其它扩展**提供了 `SubagentWorkflow` / `Workflow` / `workflow` 之一
  （精确匹配，避免误伤 `github_workflow_run` 这类）；自己用 `Extension::name()` 排除
  （上游靠"描述不等于我自己的描述"，prux 有名字可用）。
- `auto` → 让位：`SubagentWorkflow` 从工具表消失（`filter_extension_tools` 在
  `compose_tools` 与 `get_all_tools` 两处都生效，因此模型看不到、孙代理也继承不到）、
  `/agents workflows` 改为报说明、运行列表与 dock 段不出现；一次性通知。
  `on` → 不让位，仅报告。`off` → 从不提供。
- 判定**惰性求值 + 缓存**（`session::verdict()`），在 `on_registered` / `on_session_start` /
  `on_session_switched` / `on_enabled_changed` / `workflows` 设置变更时失效；
  `on_registered` 与 `on_session_start` 会在锁外主动算一次并通知，惰性路径只算不通知
  （`filter_extension_tools` 会被 `compose_tools` 每次重建调多次，不能让它在渲染路径里发通知）。

### 5.10 S4b 的核心接缝（契约冻结：journal 重放与 `agent({ resume })`）

**journal**（`workflow/journal.rs`，上游 `journal.ts`）

- 位置：与脚本同在会话任务目录（`<runId>.workflow.jsonl`）。**每次运行都记**（不只是
  `resumeFromRunId` 的那次），因为"下一次能不能省"取决于这一次有没有留下记录。
- **键** = `sha256(定序 JSON 数组)[..32]`，数组 = `[prompt, label, model, agentType, effort,
  gate, resume]`，**schema 只在存在时追加**——无条件追加会改掉每条已有记录的正则形式、
  让磁盘上所有 journal 作废（上游同此；prux 少一位 `isolation`，理由见 §8.13 偏离 1）。
- **前缀语义**：按位置顺序走，任一处（位置不存在 / `index` 不符 / 键不符 / 上次 `ok=false`）
  就**永久**结束本次运行的可重放前缀。乱序复用后面的命中等于复用在不同上游条件下产出的结果。
- **`resumed` 标记整盘作废**：带 `agent({ resume })` 的 journal 一次都不重放——被重放的 agent
  是文件里的一段文本、不是活的 child，没有对话可供续，而能找回对话的 id 表属于真正 spawn 它的
  那次运行。
- 读取**从不报错**：文件缺失/被截断/被手改都只意味着"没有可重放的东西"（代价是 token，
  而拒绝运行代价是整次运行）。结算即 append，所以被掐掉的运行也留下它跑完的那些。
- 重放出来的文本**仍要过一遍 schema**（`SchemaHooks::check_text`，上游 `applySchema` 的对应物）：
  键里已含 schema，这道检查是给"没被任何东西校验过"的路径兜底（手改过的 journal、撕裂的末行）。
- 进度条目：重放的那条 `state: "done"` + **`cached: true`** + `durationMs: 0`
  （0ms 会让它看起来"快得不可能"，`cached` 就是给界面/摘要解释这件事的）；
  本次运行也把重放结果重新记进自己的 journal（"恢复的恢复"不必再回走一条链）。

**`resumeFromRunId`**（`tool.rs`，上游 `resolveResumeTarget` + 那段 `scriptPath` 兜底）

- **同会话限定**：目标必须是本会话注册表里的运行、必须已终结、必须留下过 journal；
  未知 id 报"本会话没有这次运行"并列出本会话已知的 run id。
- 本次调用**没给来源**（`script`/`scriptPath`/`name` 都没给）时，用**上一次运行的那份脚本**——
  "再跑一次、便宜点"不该要求重新说出一次运行已经知道的路径。
- 新运行有自己的 id 与 journal 文件；`resumed_from` 记在运行上，摘要里说
  "N of M agents replayed from <run>"。

**`agent({ resume: '<label>' })`**（`glue.js` + `run_one_agent`）

- 续的是"先前在**同一个 label** 下跑过的 child"：label 是脚本能看见的唯一定位（记录 id 是内部 id）。
  runner 侧维护一张 `label → 记录 id` 表（与进度条目用同一个 label 口径：显式 `label` 优先，
  否则 prompt 首行），命中就以该 id 作为 `dispatch` 的 resume 目标（复用同一条记录、打开既有子会话、
  以其上下文继续）。label 不存在 → 该 agent 失败（脚本得 `null`），错误写明是哪个 label。
- **互斥**：`resume` 与 `agentType`/`model`/`effort`/`isolation`/`gate`/`schema` 一起给会被拒
  （续的是已经存在的 child，它保留启动时定下的类型/模型/工具契约/工作树；静默忽略会看起来像生效了）。

### 5.11 S4c 的核心接缝（契约冻结：运行检查器与控制面）

**控制面**（kernel → 扩展；上游 `WorkflowControl`）

```rust
pub struct WorkflowControl { /* Arc 化的 pause/resume/is_paused/skip/retry */ }
// WorkflowInputs 新增
pub control_sink: Option<Arc<dyn Fn(WorkflowControl) + Send + Sync>>,
// AgentRequest 新增
pub abort: Arc<AtomicBool>,   // 本次尝试的取消标志
```

- kernel 在**第一个 agent 开跑前**把控制面交回（上游 `onControl` 的理由：运行**结束**时
  `runWorkflow` 才 resolve，那时已经没有东西可控制了）。扩展挂在运行记录上
  （`WorkflowRun.control`），检查器用它驱动。
- 三件事的语义（都是上游语义）：`pause()` 只停止**启动**新 agent，在跑的跑完；`skip(index)`
  让那次调用返回 `null`、行画成 skipped、journal 记成**失败**（否则下次恢复会重放它）；
  `retry(index)` 只在那个 agent **正在跑**时有效。
- **每个活跃调用一个取消标志**：kernel 在建调用时新建它，放进 `LiveCall.abort`，并通过
  `AgentRequest.abort` 交给 runner；runner 用 [`ToolExecCtx::parent_abort`] 派生（`attempt_ctx`），
  于是"跳过/重跑/掐 run"能真的**停掉那个 child**，而不只是不再启动新的。
  `retry` 会换一个新的标志再跑一次（旧的那个已经被置位）。
- 掐整次运行 = 把**全部**活跃调用的标志置位（`cancel` 检查处），子代理随之联动中止。
- `retry` 在**同一次调用**上重跑：脚本那条 promise 还在等，所以它拿到的仍是这次调用的结果
  （新答案）；条目上记 `attempt` 与 `lastAttemptReason: "user-retry"`（上游同此）。
- 暂停中的 agent 会被 `skip`/`resume` 从等待里叫醒（每个调用一个 `Notify`，`resume` 叫醒全部）。

**检查器 UI**（`workflow/inspector.rs`，上游 `ui/workflow-dialog.ts`）

- 两栏：左 = 这次运行的树（阶段分组 / 扁平两视图，`f` 切换），右 = 选中行的详情
  （状态/类型/记录 id/用量/错误/提示词与结果预览）。**全屏**覆盖层。
- **新的一点点核心接缝**：`Extension::overlay_view(&self, id, cols)` —— 核心的 `OverlayView`
  只是一列带样式的行、不认识"栏"，所以终端宽度要交给扩展，由扩展自己拼两栏（同理，
  `TUI` 在 `ShowOverlay` 那一刻还不知道宽度，先给一个保守值 80，下一帧 `sync` 用真实宽度重拉）。
- 按键（只列此刻真能用的）：↑↓ 选行、`f` 扁平/阶段、`p` 暂停/继续、`s` 跳过、`r` 重跑、
  `x` 停止、`c`/Enter 打开选中 agent 的**会话查看器**（复用 Tier2 的覆盖层）、Esc 返回列表。
- 入口：`/agents workflows` → 运行列表（↑↓ 选、Enter 进检查器）；Esc 由 TUI 关闭覆盖层。
- 运行状态多一个 `paused`（`RunStatus::Paused`）：`p` 同时置 kernel 的暂停与 run 的展示状态。

### 5.12 S4d 的核心接缝（契约冻结：带值 CLI flag）

上游唯一**带值**的 flag 是 `--subagents-workflow-file <path>`（会话启动即跑一个工作流文件）。
prux 的 `CliFlagDef` 原来只有 bool 开关，所以这一刀给核心加了带值通道：

```rust
pub struct CliFlagDef {
    pub name: &'static str,
    pub description: &'static str,
    /// `true` = 需要值（`--name <value>`），走 `apply_cli_flag_value`
    pub takes_value: bool,
}

// Extension 新增（默认空实现）
fn apply_cli_flag_value(&self, name: &str, value: &str) {}
```

- clap 侧：`takes_value` 为 true → `ArgAction::Set` + `num_args(1)`；否则维持 `SetTrue`。
  带值 flag 不给值**报错**（不静默当开关）。
- 解析结果分两路回传：`ParsedArgs.extension_flags`（bool）与
  `ParsedArgs.extension_flag_values`（`(name, value)`），main 分别分发到
  `apply_cli_flag` / `apply_cli_flag_value`。
- `subagent` 声明 `--subagents-workflow-file`：`apply_cli_flag_value` 只**记下**路径；
  真正启动推迟到拿到 [`ToolExecCtx`] 的那一刻（`on_exec_ctx`）——跑工作流必须有
  `make_sub_agent`，而它只随 ctx 来。拿不到运行时句柄时**不取走**待跑项，留给下一次机会；
  工作流被关闭（`workflows: off`）或让位时直接清掉待跑项，不在以后莫名跑起来。
- 启动复用工具调用的同一条路径（`execute_file` → `execute`），所以脚本落盘、journal、
  完成通知与卡片的行为完全一致。
- **与上游的一处差异**：prux 的扩展 flag 只在扩展**已启用**时注入，所以这个 flag 需要
  subagent 扩展先启用（上游同理：flag 与工作流开关绑在一起）。

### 5.13 S5a/S5b 的核心接缝（契约冻结：配置双层合并 / 型号解析与 scope）

**配置双层合并**（`config.rs`，上游 `settings.ts`）

- 层序：缺省 ← `agent_dir()/extensions/subagent.json`（全局）←
  `<cwd>/.prux/extensions/subagent.json`（项目）。
- **逐键**覆盖：`apply_layer` 只写上层真写了的键；**写坏的值跳过 = 保留下层**（不是回退缺省）。
- 项目层**受项目信任门控**（`project_trust::is_project_trusted`）：未受信任时整层不参与。
  项目里的文件不该在未受信任的仓库里悄悄改掉子代理的行为。
- 缓存键 = 两层的（路径, mtime）（`ConfigKey`）；任一层变化都重读。
- 层的 cwd 来自 `on_session_start` 记下的会话 cwd（`discovery_cwd`）。

**型号解析与 scope**（`model.rs`，上游 `model-resolver.ts` + `model-scope.ts`）

- 解析顺序：精确（大小写不敏感，且必须在**有鉴权**的可用目录里）→ 模糊打分
  （`id`/`provider/id` 子串按紧密度加分，其次 `name`；`.`↔`-` 归一化；尾部 8 位日期戳可选）
  → 提供方回退（`provider/x` 没命中时拿 `x` 再试所有提供方）→ 报错并列出可用型号。
- **解析在扩展侧，核心仍只接受精确 `provider/id`**：这样"人的写法"与"核心的精确性"各就各位。
- scope：`scope_models`（缺省关）开启且用户有 `enabledModels` 时，
  **调用方**给的越界型号 → 拒绝（`dispatch` 返回 `Err`，错误里列出允许的型号）；
  **frontmatter** 钉的越界型号 → 告警一次后继续。
- 型号来源由 `SpawnRequest.model_source`（`Caller`/`Frontmatter`/`Inherited`）携带——
  合并发生在工具层（`ty.model.or(caller model)`），只有那里知道是谁定的。
- 可用型号目录进程内缓存一次（列目录要遍历提供方与 OAuth 凭据，而解析发生在每次 spawn 上）。

### 5.14 S5c 的核心接缝（契约冻结：定时子代理）

**四种调度串**（`schedule/parse.rs`，上游 `detectSchedule`）

判定顺序即上游顺序（`+10m` 与 `5m` 都是"数字+单位"，靠前导 `+` 区分）：

| 写法 | 类型 | 语义 |
|---|---|---|
| `+10m`/`+1h`/`+2d`/`+30s` | `once` | 相对一次性（规范化成目标时间戳） |
| `5m`/`1h`/`2d`/`30s` | `interval` | 固定间隔 |
| ISO 时间戳 | `once` | 绝对一次性；**过去的时间直接拒**（不造"永远不会跑"的记录） |
| 6 段 cron | `cron` | `秒 分 时 日 月 周`（`0 0 9 * * 1` = 本地周一 9:00） |

- cron 段支持 `*`、`a`、`a-b`、`*/n`、`a-b/n`、逗号列表；周日写 `0` 或 `7` 都行。
- **下一次触发按本地时间算**（`chrono::Local`；`0 0 9 * * 1` 对用户就是本地周一 9 点）。
  日/周字段都受限时按 cron 老规矩取**或**。夏令时跳过/重复的钟点：取最早的那个。
  扫描上限 4 年，找不到（如 2 月 30 日）返回 `None` 而不是死循环。
- 解析不出来时报错并给三种写法各一个例子。

**存储**（`schedule/mod.rs`）

- 会话作用域：`<agent_dir>/extensions/schedules/<会话文件 stem>.json`（没有落盘文件的会话用 `default`）。
  `/new` 起空的，`/resume` 读回——与上游"每会话一份"一致，只是放在 agent 目录而不是项目里。
- 原子写（tmp + rename）：读的人永远看不到半截 JSON。**没有 PID 锁**（上游那套是给多进程用的；
  prux 单进程，改动就是"读-改-写"）。

**调度器**

- 一个 **1 秒 tick** 的扫描任务（不是每任务一个定时器）：判断只有一处
  （`next_run_ms <= now`），会话切换/新增/取消都不会漏或重复武装。
- 到点 → **走同一条 `manager::dispatch` 路径**：后台运行、完成通知、卡片、记录全都复用
  （上游注释同此："delivery is implicit"）。
- 定时 spawn 带 `bypass_queue`：**不占后台额度也不排队**。每 5 分钟一次的活儿不该排在
  4 个长任务后面（周期会彻底失真）——配额是给人的突发行为用的，不是给墙钟用的。
- 记账：触发前记 `running` + `last_run`，收尾按记录状态记 `success`/`error`、`run_count` +1、
  重算下一次；**一次性任务跑过自动关停**；落盘的过期一次性任务标错误 + 关停并告警。
  类型解析失败/没有可派发的上下文 → 记 `error` + 告警（不静默、不 panic）。
- ticker 用"**代**"而不是布尔：旧会话的任务醒来发现代变了就退出，新会话不会被它挡住。

**入口**

- `Agent` 工具的 `schedule` 参数（**启用时才进 schema**：关掉时零上下文成本）：
  注册任务而**不**现在跑；拒绝与 `resume`/`inherit_context: true`/`run_in_background: false` 组合；
  agent 类型在创建时校验（到点会再解析一次——用户可能改了文件）。
- 提示词里只在启用时提这条（对齐上游 `scheduleGuideline`："只在用户明确要求定时/周期/延迟时用"）。
- `/agents schedules`：列表（↑↓ 选、`d` 取消；选中行下面是详情）+ `schedule: off` 时的说明。
- 设置项 `schedule`（缺省开）。

### 5.15 S6a 的核心接缝（契约冻结：git worktree 隔离）

`isolation.rs`（`worktree.rs`）不需要新的核心接缝：`SubAgentSpec.cwd` 本来就能指定工作目录。

- **创建**（`create`）：`git rev-parse --is-inside-work-tree` / `HEAD` / `--show-toplevel`，
  然后 `git worktree add --detach <tmp>/prux-agent-<id>-<pid> HEAD`。分支名 `prux-agent-<id>`。
  调用方 cwd 在仓库里更深时，副本里的工作目录是对应的子目录（两侧 realpath 后求相对路径），
  monorepo 包的改动范围不会悄悄扩大到整个仓库。
- **严格失败**：不在仓库里 / 还没有 commit / `worktree add` 失败 → **报错**，不回退到主目录。
  悄悄在主目录跑等于让调用方以为自己在隔离环境里。
- **收尾**（`cleanup`）：`git status --porcelain`；有改动 → `add -A` + `commit --no-verify`，
  再建分支（同名已存在就加时间戳后缀），然后 `worktree remove --force`；干净且 HEAD 未动 →
  直接删。任何一步失败都尽力删掉副本、如实报"没留下分支"，不把跑完的结果变成失败。
- **结果里点名分支**：有改动时结果正文追加
  `[isolation: worktree — changes committed to branch <b> in the main checkout]`，
  记录里也留 `worktree_result`。
- **所有 git 调用都带超时**（`git worktree add` 30s），走 `tokio::process`：
  一次副本可能要几秒，同步调用会把 UI 线程一起堵住。
- **参数与开关**：`Agent({isolation: "off"|"worktree"})`（`worktree_isolation` 关掉时**整个参数
  不进 schema**，与 `schedule` 同一取舍）；frontmatter `isolation:` 可钉死（优先级高于调用参数）。
  开关关掉时**丢弃**请求（不报错）——那是用户在关掉能力，不是调用者的错。
- **孤儿清理**：会话开始 `git worktree prune`（上次进程崩了留下的注册项）。

### 5.16 S6b 的核心接缝（契约冻结：嵌套子代理）

**一句话**：委派工具的名字不变、作用域在运行期按**调用者身份**决定；身份由扩展声明、核心原样带回。

**核心侧（两处小改动，都不是新概念）**

1. `SubAgentSpec` 增 `agent_id: Option<String>` / `depth: u32`；核心把它们写进 child 的
   `ToolRebuildCtx`，再由 `ToolExecCtx` 交回给扩展（`agent_id`/`depth`）。
   核心**不解释**这两个值——它只是让扩展知道"现在说话的是谁、在第几层"。
2. 递归防护多一个例外：`SUBAGENT_TOOL_NAMES` 仍默认剔除，但**显式列在 `spec.tools` 里的**保留。
   那是扩展为嵌套委派自己放行的（`build_spec` 在深度允许时把三个名字写进 child 的白名单），
   不是 child 顺手继承来的。用户若在 agent 文件里写 `tools: Agent` 也会走这条路——
   但 handler 里的深度守卫照样拦（见下）。

**扩展侧**

- `nested.rs`：`NESTED_TOOL_NAMES`（与顶层同名）、`nesting_allowed(depth, max_depth)`、
  `depth_exceeded`。深度上限 `max_subagent_depth`（缺省 2；`0`/`1` = 关掉嵌套），
  **作为参数**传进 `build_spec`（那个函数因此不碰全局配置，测试可钉住上限）。
- `manager::caller_of(ctx)`：直接读 ctx 的 `agent_id`/`depth`（主会话 = None/0）。
- `manager::owned_record(caller, key)`：子代理**只能**看见自己派发的那些
  （`record.parent_agent_id == caller.agent_id`；主会话不受限）。`get_subagent_result` /
  `steer_subagent` 在子代理调用时走这个作用域。
- `Agent` 工具：嵌套时 `parent_agent_id = caller.agent_id`、`depth = caller.depth + 1`；
  **嵌套默认前台**（后台的子孙会随父代理结束被掐掉，等于把活儿丢掉——上游同此）。
- 父代理收尾 → `abort_owned_children`：掐掉自己派发的、仍在跑的子孙（上游 `abortOwnedChildren`）。
- dock 里嵌套代理缩进一级（`↳`），一眼看出谁派的谁。

**与上游的差异（S6b 时）**：当时 `allowed_subagents` 仍未支持（在 `UNSUPPORTED_FIELDS`）。
**S7b 已收口**：它现在是**嵌套开关 + 类型白名单**（`None` = 不开启；`Some([])`/`all` = 开启不限型；
`Some(list)` = 开启且限型），`build_spec` 按它放行委派工具（见 §3.8 / §11.5 H1）。

### 5.17 S6c 的核心接缝（契约冻结：扩展↔扩展事件总线）

**一句话**：核心只提供"按字符串通道名广播 `(name, payload)`"的哑总线；谁能发/谁能收由
扩展生命周期（已启用 + 当前模式可用）门控；`requestId`/回执信封是**扩展之间的线协议**，核心不认识。

**核心侧**

1. `hooks.rs`：新增 `ExtensionHook::ExtensionEvent`。**订阅 = 声明这个 hook**（沿用既有门控契约，
   不引入第二套订阅注册）。
2. `Extension` trait：新增 `fn on_extension_event(&self, _name: &str, _payload: &Value) {}`。
3. 新模块 `core/extensions/events.rs`：
   - `pub fn emit(from: &str, name: &str, payload: Value)`：校验 `from` 是"已注册且启用"的扩展，
     然后把事件快照投递给所有 `registered()`（已启用 + 模式可用）且声明了 `ExtensionEvent` 的扩展。
   - `pub fn dispatch(name: &str, payload: &Value)`：内部投递（快照后逐个调用）。
   - **重入队列化**：handler 内再 `emit` 时，事件进队列，等**当前 dispatch 返回后**按 FIFO drain；
     handler 永不嵌在另一个 handler 里跑（锁序固定 `registry → 扩展 state`）。非 handler 上下文
     （后台任务）里 `emit` 立即投递。drain 有上限（1024 个/连锁）防事件风暴。
   - payload = `serde_json::Value`；**不按会话隔离**（谁关心哪个会话由 payload 里的 `sessionId` 自己过滤）。
   - 发送方也会收到自己的事件（不特判；通道名由扩展自己区分）。

**扩展侧（subagent）**

- 按上游发 `subagents:*` 生命周期事件：`ready` / `created` / `started` / `completed` / `failed` /
  `steered` / `compacted` / `scheduled` / `scheduler_ready`；payload 含 `sessionId` 等上游字段。
- RPC 四方法挂在同一总线上，回执通道 `"<channel>:reply:<requestId>"`，信封
  `{success:true,data?}` / `{success:false,error}`：
  - `subagents:rpc:ping` → `{version: PROTOCOL_VERSION}`（= **2**）；
  - `subagents:rpc:spawn` `{requestId,type,prompt,options?}` → `{id}`；`options.model` 字符串
    先 fuzzy 解析再 scope 检查（caller 级别=硬错误）；用扩展缓存的活跃 ctx，无则 `No active session`；
  - `subagents:rpc:stop` `{requestId,agentId}` → 仅顶层记录（`Agent not found` / `Agent is not running`）；
  - `subagents:rpc:consume` `{requestId,agentId}` → 标记已读，抑制完成通知（`Agent not found or still running`）。
- `on_extension_event` 是同步的；`spawn` 需要 await startup，故 handler 内 `tokio::spawn` 完成任务后 emit 回执。

### 5.18 S6d 的核心接缝（契约冻结：mention clone）

**一句话**：扩展要能把父会话"模型真正在用的那份上下文"（压缩感知）克隆进一个一次性会话，
让副本在屏幕外跑一个**只带 `Agent`、绑定主上下文、强制后台**的回合。

**核心侧**

1. **实时上下文快照**：核心提供一个"当前会话上下文"读取入口，返回模型真正在用的那组消息
   （压缩已折叠）+ 系统提示 + 生效型号。实现落在 `ToolExecCtx`（连同 `parent_model`）。
2. **隐藏回合**：核心提供"用给定上下文 + 仅 `Agent` 工具跑一个一次性回合、且其工具执行上下文
   归属**主会话**（顶层 spawn）、强制后台"的原语；该回合不落主会话、不出现在 widget。

**扩展侧（subagent）**

- `agent_mentions=model`：`@handle` 认领回合 → 克隆会话 → 副本拿 `message + <system-reminder>` 提示
  跑隐藏回合 → 顶层后台 spawn；失败回落 direct 并告警。
- `agent_mentions` 默认从 `direct` **收敛回 `model`**（对齐上游）。
- 不再有"model 未实现"的降级告警。

### 5.19 memory（无核心接缝，extension-only 契约）

- 三档路径：user=`<agent_dir()>/agent-memory/<name>/`，project=`<cwd>/.prux/agent-memory/<name>/`，
  local=`<cwd>/.prux/agent-memory-local/<name>/`。
- project/local **读写都要求项目信任**（未受信=不注入 + 一次性告警，绝不写）；不做旧路径兼容。
- 读写分支：生效工具含 `write`/`edit` → 补 `read`/`write`/`edit` + 完整 memory 块；
  否则只补 `read` + 只读块。一律注入 `append_system_prompt`，不动 `system_prompt`。
- 非法 agent 名拒绝、symlink 目录/文件拒绝（读与写）、MEMORY.md 200 行截断。
- 只由 frontmatter `memory` 触发，不加全局开关。

---

## 六、测试对照表

上游 97 个测试文件按 Tier 归属如下。Tier1 需要"有对应用例"或"明确记为无对应用例"；
**实际实现的 prux 用例名与数量见 §8.3**。

### 6.1 Tier1 必须有对应用例（prux 侧用例位置）

| 上游测试 | 覆盖的行为 | prux 用例 |
|---|---|---|
| `agent-types.test.ts` | 类型解析/覆盖/禁用/大小写 | `subagent/agent_types.rs` inline（`AgentDirGuard` + tempdir） |
| `custom-agents.test.ts` | 两层发现、frontmatter 解析、坏文件跳过 | `subagent/agent_types.rs` inline |
| `agent-file-bom.test.ts` | agent 文件 UTF-8 BOM | `subagent/agent_types.rs` inline（prux 已有 `utils/mime.rs` BOM 先例） |
| `documented-defaults.test.ts` | 默认三类型的存在与工具集 | `subagent/agent_types.rs` inline |
| `prompts.test.ts` | replace/append 两模式 + `<active_agent>` | `subagent/prompt.rs` inline + `tests/subagent.rs` 断言系统提示 |
| `agent-manager.test.ts` | 生命周期、并发排队、结果表 | `subagent/manager.rs` inline（fake runner） |
| `agent-manager-gc.test.ts` | 记录回收 | `subagent/manager.rs` inline（Tier1 简化：会话切换即清空） |
| `wait-queued.test.ts` | `get_subagent_result(wait)` 对排队者的等待 | `subagent/manager.rs` inline |
| `foreground-result-retrieval.test.ts` | 前台结果 inline 取回 | `tests/subagent.rs`（mock HTTP 端到端） |
| `steer-subagent-wiring.test.ts` | steering 注入与状态 | `subagent/manager.rs` inline + 端到端 |
| `agent-tool-error-rendering.test.ts` | 工具错误文本 | `tests/subagent.rs` 断言错误文本 |
| `agent-startup-error.test.ts` | spawn 失败（模型/类型解析失败） | `subagent/manager.rs` inline |
| `model-resolver.test.ts` | 精确解析 | `subagent/manager.rs` inline（**只覆盖精确路径**，模糊部分见 6.3） |
| `usage.test.ts` / `usage-reporting.test.ts` | token 累加 | `subagent/manager.rs` inline（feed 合成事件） |
| `status-note-wiring.test.ts` | 异常状态说明 + 部分结果 | `subagent/notify.rs` inline |
| `child-session-shutdown.test.ts` | 会话/扩展关闭时停止 child | `subagent/manager.rs` inline（断言 abort 被置位） |
| `foreground-concurrency.test.ts` | 前台并发 | `tests/subagent.rs`（一条消息两个 `Agent`） |
| `subagents-print-mode-e2e.test.ts` | 端到端 | `tests/subagent.rs`（prux 只有 TUI 模式，故只覆盖"父级→child→父级"链路） |
| `manager-registry-guard.test.ts` | 重复注册/注销保护 | `subagent.rs` inline（对齐 `agent_session.rs` 既有 `SUBAGENT_TEST_LOCK` 模式） |

### 6.2 上游用例在 Tier1 需**改写**的

> 注：`background-by-default` 的断言在 Tier2 已收敛回上游语义（默认后台，见 §3.1），
> 当前用例断言的是"参数缺省时取配置值、默认后台"。

| 上游测试 | 改动 |
|---|---|
| `background-by-default.test.ts` | Tier1 曾断言默认前台；Tier2 后改为断言"缺省取配置 `background_default`（默认后台）"+ 显式 `run_in_background: false` 为前台（§3.1） |
| `foreground-concurrency-wiring.test.ts` | 断言"同一条消息多个 `Agent` 调用并发" |
| `nested-tools.test.ts` / `nested-delegation-e2e.test.ts` | 递归防护用例（child 白名单不含三个子代理工具）+ S6b 的深度白名单放行用例 |

### 6.3 Tier1 明确无对应用例（推迟到 TierN 或决策不迁移）

> 此表是 Tier1 时的规划；"归属 Tier2/3/4"的项已在后续切片交付并补了用例，
> 实际落点见对应 §8.x。仍为无对应用例的只有 `perf/*` 与已明确不迁移的项。

| 上游测试 | 归属 |
|---|---|
| `group-join.test.ts` / `clear-completed-wiring.test.ts` | Tier2（分组通知） |
| `agent-widget.test.ts` / `fleet-list.test.ts` / `fleet-wiring.test.ts` / `conversation-viewer*.test.ts` | Tier2（UI） |
| `agent-color*.test.ts` | Tier2（badge 渲染；Tier1 只解析 `color` 值） |
| `workflow-*.test.ts`（14 个） | Tier3 |
| `mention*.test.ts` / `agent-mention*.test.ts` | Tier4 |
| `memory*.test.ts` / `skill-loader.test.ts` / `output-file*.test.ts` | Tier4 |
| `worktree*.test.ts` | Tier4 |
| `schedule*.test.ts` | Tier4 |
| `cross-extension-rpc*.test.ts` / `rpc-*.test.ts` | Tier4（需先造 prux 扩展事件总线） |
| `model-scope.test.ts` / `enabled-models.test.ts` / `model-resolver.test.ts`（模糊部分） | Tier4 |
| `settings.test.ts` / `tool-description-mode.test.ts` / `strict-agent-files-wiring.test.ts` / `fallback-subagent-wiring.test.ts` | Tier4 |
| `isolation-param.test.ts` / `invocation-config.test.ts`（isolation 分支） | Tier4 |
| `abortable.test.ts` | ➖ 3.13 |
| `structured-output.test.ts` | Tier3 |
| `context.test.ts` | Tier1 部分覆盖（`inherit_context`）；其余 Tier4 |
| `agent-file-toggle.test.ts` | Tier4（eject） |
| `agent-runner-settings.test.ts` | Tier4 |
| `perf/*` | ➖ 不做基准移植（prux 无对应基准基建） |

### 6.4 需要改写或新增的 prux 既有用例（实际落点）

完整清单（含每个用例名与数量）见 §8.3；此处只列与既有测试的交接关系。

| 位置 | 实际改动 | 状态 |
|---|---|---|
| `core/agent_session.rs::subagent_extension_runs_task_tool` | 改名 `subagent_extension_runs_agent_tool`、改用 `Agent` 工具、用 `AgentDirGuard::temp()` 隔离子会话落盘、并断言状态头与 `details` | ✅ |
| `core/agent_session.rs::sequential_task_batch_assigns_source_order_tool_indexes` | 改名 `sequential_extension_batch_assigns_source_order_tool_indexes` 并改用本地 `seq_probe` 扩展工具：`Agent` 现在是 `Parallel`，不再触发顺序路径；被验证的 tool_index 源序语义与 `reduce_lane` 恢复校验保持不变 | ✅ |
| `core/agent_session.rs` | 新增 7 个（spec 物化 5 + 前台端到端 1 + 后台端到端 1），覆盖工具白名单/递归防护/两种提示模式/`inherit_context`/型号解析失败/max_turns 回调/resume 回灌/后台通知与子会话挂父会话 | ✅ |
| `core/session_manager.rs` | 新增 4 个（唯一构造入口与薄包装等价 / `parent_session_id` 落盘 / `persist:false` / 非法 id） | ✅ |
| `extensions/web_access.rs::fetch_content_rejects_unsupported_params` | 手工构造的 `ToolExecCtx` 改为 `make_sub_agent`/`parent_abort` 桩（`tests/web_access.rs` 同步） | ✅ |
| `tests/extensions_registry.rs` | **无需改动**：它只断言扩展注册表与工具查询，不依赖具体工具名；已跑通 | ✅ |
| `extensions/plan_mode.rs::turn_end_persists_only_on_progress` | 修正既有顺序依赖（`ensure_registered()` 后 `drain_ui_requests()`），理由见 §8.5 | ✅ |
| `tests/subagent.rs` | 新增 6 个黑盒用例（不联网） | ✅ |

---

## 七、已知缺口与未决项

### 7.1 JS 沙箱引擎选型（✅ 已用 spike 结案：rquickjs）

**结论**：`rquickjs 0.12.2`（QuickJS）。**成功标准已达成**：上游文档里的
`auth-audit` 示例脚本（原文照抄，仅去掉 `export`）原样跑通——顶层 `await`、顶层
`return`、`phase()`、`log()`、`pipeline()` 两段、`Promise.all` 并发、JSON 边界回传。

**spike 证据**（一次性原型，`/tmp/jstest`，未入库）：

```
LOG: auditing 2 route files
PHASES: ["Scan", "Audit"]
RETURN: {"files":["src/routes/auth.ts","src/routes/users.ts"],"findings":[…两条…]}
OK: parallel()/pipeline()/phase() + 顶层 await/return 全部跑通
```

**已验证的架构（Tier3 按此实现）**：

1. `AsyncRuntime::new()` + `AsyncContext::full(&rt)`；用
   `ctx.async_with(async |ctx| { … }).await` 作为**唯一**驱动入口。
2. JS 侧桥：`__call(method, payload)` 建 Promise 存进 `Map`，再同步调用宿主注入的
   `__dispatch(id, method, json)`；宿主处理完用注入的 `__settle(id, ok, json)` 结算。
3. Rust 侧驱动循环（在 `async_with` 内部）：
   `drain 宿主请求队列 → await 真实异步工作（可并发）→ __settle → 再 drain`，
   空闲时 `select!` 等主 Promise。`WithFuture` 在内部 future 挂起时会自动跑
   JS 任务队列（jobs + spawner），所以宿主 await 不会饿死 JS 侧 Promise。
4. **边界是 JSON 字符串**（rquickjs 0.12 没有 serde-json 集成）：宿主→JS 用
   `JSON.parse`，JS→宿主用 `JSON.stringify`（顺带天然拒绝循环引用/函数）。
5. **确定性**：`Date.now` / `Math.random` / `new Date()` 在**前奏**里改写为抛错
   （上游做法，可直接抄字符串）。
6. **可中止**：`AsyncRuntime::set_interrupt_handler` + 宿主侧 abort 标志/超时
   （替代上游"worker.terminate 能杀掉 await 中的脚本"）。
7. `export const meta = {…}` **不执行脚本**、按上游 `meta.ts` 的字面量提取校验。

**代价（已知并接受）**：需要 C 编译器（QuickJS 以 C 源码内联编译）；
`bindgen` feature **不必要**（预生成绑定即可，无需 libclang——两者都实测通过离线构建）。
体积：debug 二进制 ~19MB（含 tokio 等），release 未测；需要时再加
`opt-level`/`lto` 观察。

**仍未决（Tier3 内部，实施时逐条定）**：`gate`（跑命令验证）与 `resume`
（复用上下文）的宿主语义、journal 重放的键（脚本哈希 + 阶段前缀）、
具名工作流目录（上游 `.pi/workflows/` → prux 侧建议 `.prux/workflows/` 全局
`<agent_dir()>/extensions/workflows/`）、与外部 `Workflow` 工具的让位检测。

### 7.2 覆盖层与卡片原语（✅ Tier2 已按建议落地；仅一处剩余）

**结论**：采纳"一个通用覆盖层原语"方案（而不是给 dock/panel 打补丁），实现见 §5.5。
FleetView = `Inline`，查看器 = `Fullscreen`，卡片 = `CustomMessage` + `render_custom_message`。
原先的三个未决点按上游语义定案：FleetView 占输入框下方、查看器全屏、Esc 必返、
覆盖层内键位**扩展优先**（未消费才走 TUI 默认）。

**唯一剩余（已收敛）**：查看器滚动键现已接用户 keybindings（`tui.select.up/down/pageUp/pageDown`
→ `OverlayKey`，见 row 82 / §8.25）；默认仍 ↑↓/PgUp/PgDn。
补法：把 `OverlayKey` 映射接到 `core/keybindings.rs` 的按键解析（滚动/翻页/首末行），
不改变扩展侧接口。以下是当时的决策记录（保留供对照）。

Tier2 切片 1（widget / 设置 / 菜单 / 颜色 / 默认后台 / 结果卡片）**不需要**新核心原语：
widget 复用 `dock_lines`，菜单复用 `Select`+`ShowSettings`，卡片复用 `NotifyRich`。
下列三项做不到，必须动核心 TUI：

1. **FleetView（编辑区下方可导航列表）**
2. **会话查看器（全屏浮层 + 滚动 + 内联 steer/停止输入）**
3. **完成通知卡片（自定义消息渲染类型：`Continuation` 降级为精炼文本 + 卡片承担展示）**

**建议方案（倾向"一个通用覆盖层原语"，而不是给 dock/panel 打补丁）**：

| 原语 | 形态 | 为什么 |
|---|---|---|
| `ExtensionUiRequest::Overlay { id, title, lines, footer }` | TUI 在输入框上方/全屏绘制扩展提供的带样式行；键盘（↑/↓/j/k/PgUp/PgDn/Esc/Enter/自定义键）经新钩子 `Extension::on_overlay_key(id, key) -> OverlayAction`（`Redraw`/`Close`/`Capture`/`Prompt(text)`）回传 | FleetView 与查看器只差"内容 + 键位"；一个原语覆盖两者，扩展自持导航状态（对齐"核心给机制、扩展给策略"） |
| 覆盖层内联输入 | `OverlayAction::Input { label }` → TUI 复用既有输入行，提交后经 `on_overlay_input(id, text)` 回传（steer 用） | 避免为 steer 单独造 dialog |
| 自定义消息渲染 | `Extension::render_message(&self, custom_type: &str, data: &Value) -> Option<Vec<RichSpan>>` + `ExtensionUiRequest::AppendCustomMessage { custom_type, data }`（落盘走既有 `PersistSessionEntry`） | 让后台完成发送"卡片条目 + 精炼 `Continuation`"，聊天区不再重复 |

**未决点**（需要拍板后再动手）：
- 覆盖层是**接管全屏**还是**占据输入区上方 N 行**（上游 FleetView 是后者、查看器是前者）？
- 覆盖层键位冲突策略：扩展按键是否需要先过内置 keybindings（上游 viewer-keys 走用户配置）？
- 自定义消息渲染是否要对齐 prux 既有 `customMessage*` 主题键（`theme.rs` 已有
  `customMessageBg/Label/Text`，但当前只有少数内置条目用）？

**不做也可以**：FleetView/查看器是"更好的可观测性"，缺它们不影响子代理功能闭环
（widget + 菜单 + `get_subagent_result` 已可查全部信息）。

### 7.3 prux 侧曾经缺失、现已补齐的基础设施

> 下表是 Tier1 动工前的盘点，S2–S6 已逐项补齐；保留供对照（真·剩余见 §11）。

| 能力 | 当初的缺口 | 现状 |
|---|---|---|
| 扩展事件总线 | prux 只有 `on_agent_event`（宿主事件流入），没有扩展间广播 | ✅ S6c：`core/extensions/events.rs` + `ExtensionHook::ExtensionEvent` |
| 扩展配置持久化 | 无"全局 + 项目双层合并的插件私有配置"模式 | ✅ S5a：`subagent/config.rs` 两层逐键合并 + 信任门控 |
| git worktree 操作 | 只有 footer 的 git 分支读取 | ✅ S6a：`subagent/worktree.rs` |
| 项目级资源的信任门控 | 无扩展把 `project_trust` 用于项目级定义 | ✅ Tier1 起：`PRUX_RESOURCES` 增 `agents` |
| `@` 名册与插入 | 无"代理名册"数据源 | ✅ S6d：`ExtensionHook::Suggestions` + `subagent/suggest` 并入 `@handle`/`@type` |
| cron 调度 | 无 | ✅ S5c：`subagent/schedule/` |

### 7.4 Tier1 实施后的实际结论（原"粗糙点"逐条结算）

1. ~~`tools()` 用进程 cwd 发现项目类型~~ → **已修正**：改为在 `on_session_start` 记下会话 cwd
   （见 5.4 的能力补强）。spawn 一律用 `ToolCtx.cwd` 重新发现，二者不会不一致到派发错类型。
2. ~~告警去重表每会话不重置~~ → **已实现为每会话重置**：去重表放在扩展状态里，
   `on_session_switched` / 禁用时随注册表一起清空。
3. `agent_types.rs` **每次 spawn 现扫目录**（默认 → 全局 → 项目）：目录很小，换取"改 `.md`
   立刻生效"。代价是每次 spawn 一次 `read_dir` + 若干 `read_to_string`；Tier2 的 widget
   若需要高频刷新，再引入 mtime 缓存（保持不变）。
4. `Agent` 工具描述里的类型清单在 **`tools()` 求值时**生成，因此改 `.md` 后描述滞后到下次
   rebuild；spawn 本身不受影响（保持不变）。
5. `/agents` 的 `busy_safe = true` 依赖 handler 只读扩展状态这一约定；实现守约
   （只读注册表、`Select` 面板与通知，不锁 agent）。**这是新增的约束，改动 `/agents`
   handler 时必须维持**。
6. 新增：**`request_ui(Continuation)` 的推送发生在持有注册表锁之外**（`notify_by_id` 先取快照
   再投递），否则会与 sink 回调里的注册表加锁形成反向锁序。
7. 新增：`verbose` 的 200 行/8KB 上限仍是经验值，未做真实长会话验证。
8. 新增：`reset_all()` 会把后台额度计数归零，但**已经 spawn 的任务仍在飞**（它们收尾时
   `running_bg` 已 saturating 到 0，不会重复计数，也不会再发通知）。会话切换瞬间可能短暂
   超发额度，Tier1 接受。
9. Tier2 结算：`widget_lines()` 每帧只取一次状态锁 + 一次 `stat`（配置 mtime 判定），
   克隆 ≤50 条记录后即释放锁；渲染层只做 `truncate_display`。**未做真实长会话/多代理
   （10 并发 + 50 条已完成）下的帧耗时测量**——若出现掉帧，先加"每帧最多 N 行"或
   仅在内容变化时才重建（当前策略：每次全量重建，实现简单）。
10. Tier2 结算：配置缓存键是 `(路径, mtime)`。路径参与判键是为了让测试的线程本地
   `agent_dir` override 与生产路径互不污染（`State` 是进程级单例）。

---

## 八、进度勾选表

### 8.1 核心改动 ✅

- [x] `core/extensions/tools.rs`：`SubAgentSpec` / `SubAgentControls` / `SubAgentRunner` /
      `MakeSubAgentFn` / `SubAgentEventSink`；`ToolExecCtx` 增 `make_sub_agent` + `parent_abort`；
      删除两个旧闭包类型
- [x] `core/agent_session.rs`：`make_exec_ctx` 改为构造 `make_sub_agent`；`materialize_sub_agent`
      按 spec 物化；递归防护（`SUBAGENT_TOOL_NAMES` 强制剔除）；max_turns 优雅收尾
      （`SUBAGENT_GRACE_TURNS`）；事件 sink 注入；父会话 id 自动填充
- [x] `core/session_manager.rs`：`SessionCreateOptions` + `create_with` + `parent_session_id()`
      accessor；两个旧入口退化为薄包装
- [x] `core/project_trust.rs`：`PRUX_RESOURCES` 增 `"agents"`（见 5.4 修正 3）
- [x] `extensions/web_access.rs` / `tests/web_access.rs`：手工构造的 `ToolExecCtx` 同步更新
- [x] `Cargo.toml`：tokio 显式启用 `sync`（`watch` 完成信号）与 `time`（前台 abort 联动轮询）

### 8.2 扩展实现 ✅

- [x] `subagent/types.rs`：`AgentType` / `SpawnRequest` / `AgentRecord` / `AgentStatus` /
      `AgentSource` / `PromptMode` / `UsageTotals`
- [x] `subagent/agent_types.rs`：两层发现 + 信任门控 + 手写 frontmatter 解析 + 默认三类型 +
      覆盖/禁用 + 类型解析 + 陌生字段与坏文件告警
- [x] `subagent/prompt.rs`：默认三类型提示词 + 桥接段 + `Agent` 工具描述（动态清单）+
      snippet / guidelines + `active_agent` 标签
- [x] `subagent/manager.rs`：注册表 + 并发池 + FIFO 排队 + abort/steer + resume + 通知投递 +
      用量累加 + id 分配 + 前台 abort 联动
- [x] `subagent/notify.rs`：`Continuation` 信封 + 状态头 + 转写整形（200 行/8KB 上限）
- [x] `subagent.rs`：工厂 + `Extension` impl（三个工具 / `/agents` 四个入口 / 钩子接线）
- [x] `extensions.rs`：`PRIORITY_SUBAGENT` 未变；`tests/extensions_registry.rs` 无需改动（通过）
- [x] `core/extensions/dock.rs`：`DockSpan::fallback` 由 `&'static str` 放宽为
      `Cow<'static, str>` + 新增 `DockSpan::hex`（动态 badge 色的唯一需要）；渲染层
      `render/dock.rs` 的"空 key = 默认前景"判定改为"key 与 fallback 都空"
- [x] `test_support.rs`：新增 `clear_search_credential_env()` / `SearchEnvGuard`
      （见 8.5：测试自带 env 隔离，不再需要 `env -u ... cargo test`）

### 8.3 测试 ✅

- [x] `subagent/agent_types.rs` inline（9 个：frontmatter/BOM/数组与逗号列表/`:` 拒绝/默认值/
      大小写与歧义/项目覆盖全局与信任门控/坏文件跳过/`enabled:false` 禁用默认类型）
- [x] `subagent/manager.rs` inline（10 个：spec 构造/replace+append/字段透传/前台 agent 跟随
      父级 abort/满额排队+排队期 steer+stop/`wait` 对排队者/后台完成释放额度并投递
      `Continuation`/resume 拒绝非终结者/未知 id 与告警去重/`reset_all`）
- [x] `subagent/notify.rs` inline（4 个：token 口径与人类化/信封/缺结果/转写渲染与行数上限）
- [x] `subagent/prompt.rs` inline（3 个：类型清单只列启用项/空名册/标签转义）
- [x] `subagent.rs` inline（7 个：必填入参/未知工具/缺 prompt 与类型/负 `max_turns`/
      未知 id 的普通文本/身份与工具面/命令子命令）
- [x] `tests/subagent.rs`（6 个黑盒：身份与工具面/工具 schema 与默认前台/项目覆盖全局的类型
      发现/禁用类型回退 general-purpose/查询失败是普通文本/入参校验）
- [x] 改写 `core/agent_session.rs` 的原 `task` 用例 → `subagent_extension_runs_agent_tool`；
      原「顺序批 toolIndex」用例改用本地 `seq_probe` 扩展工具（`Agent` 现在是 `Parallel`，
      不再触发顺序路径）→ `sequential_extension_batch_assigns_source_order_tool_indexes`
- [x] 新增 `core/agent_session.rs` 7 个：工具白名单与递归防护 / 两种提示模式与
      `clear_context_files` / `inherit_context` / 型号解析失败 / graceful max_turns 回调 /
      resume 回灌上下文 / **后台端到端**（真 LLM 轮次 + `Continuation` + 子会话挂父会话）
- [x] 新增 `core/session_manager.rs` 4 个：唯一构造入口与薄包装等价 / `parent_session_id`
      落盘与 `/resume` 可见 / `persist:false` 不落盘 / 非法 id 拒绝
- [x] `extensions/plan_mode.rs`：修正一个**既有**顺序依赖测试（见下）

### 8.5 实施期发现的既有问题（Tier1 + Tier2 累计，均已修正）

1. **`plan_mode.rs::turn_end_persists_only_on_progress` 的顺序依赖**（Tier1 发现，Tier2 修对）。
   测试依赖"本进程内 `ensure_registered()` 是否首次注册"：首次注册会经
   `on_enabled_changed(false) → persist()` 置 `write_in_flight = true` 并排入一条
   `PersistSessionEntry`。Tier1 的修法是"`ensure_registered()` 后 `drain_ui_requests()`"，
   但**直接丢弃队列项会让 `write_in_flight` 永久卡住**（回执永不到来），于是该用例
   单独跑/整包跑时随机失败（后续 `persist()` 全被吞）。
   Tier2 修法：`drain_ui_requests()` 丢弃 `PersistSessionEntry` 时补一次
   `on_persisted(CUSTOM_TYPE)`（等价"回执已到"）。**教训：删队列项必须成对处理在途标记。**
2. **subagent 测试互斥缺失**（Tier1 埋伏，Tier2 暴露）。`core/agent_session.rs` 用自己私有的
   `SUBAGENT_TEST_LOCK`，而 `subagent/manager.rs` 用 `manager::test_lock()`——两把锁互不相干，
   于是"注册表 + `reset_all`"跨文件互踩（表现为 `sub-agent … disappeared before it could be
   reported`、`record(...).unwrap()` 空）。Tier2 修法：`subagent.rs` 导出
   `#[cfg(test)] pub(crate) fn test_lock()`（转发 `manager::test_lock()`），
   `agent_session.rs` 的 4 个用例改用它。
3. **web_access 的两个用例依赖本机环境变量**（既有失败）。`Provider::is_available` 在配置未写
   密钥时回退读 `TAVILY_API_KEY`/`EXA_API_KEY`，开发者机器上已设置 → 断言"Tavily 不可用"失败。
   以前的复现方式是 `env -u TAVILY_API_KEY -u EXA_API_KEY cargo test`。Tier2 修法：
   `test_support::clear_search_credential_env()`（持叶级 `env_key_lock`、Drop 时还原）
   在 `search::tests::availability_and_routing`、
   `search::tests::auto_routing_falls_back_to_duckduckgo_when_nothing_configured`、
   `tests/web_access.rs::web_search_reports_when_no_provider_configured` 三处使用。
   **现在 `cargo test` 裸跑即全绿（1133 passed）。**
4. **`agent()` 失败语义与自己的工具描述矛盾**（S3 埋伏，S3b 修正）。`workflow/tool.rs` 的
   `TOOL_DESCRIPTION` 写着 "Returns null if the subagent fails (filter with .filter(Boolean))"，
   而 `bridge.rs` 对普通 agent 失败是 `settle(ok:false, {message})` → `glue.js` 的 `__settle` 直接
   `reject` → **`agent()` 抛异常**。在 `parallel()`/`pipeline()` 里两者都被折叠成 `null`，所以只在
   顶层 `await agent(...)` 暴露。上游是 `respond(callId, true, null)`（`runtime.ts:1124`）。
   S3b 修法：普通失败→`ok:true, "null"`，致命（配额/编译/host 方法未知）→`ok:false + fatal`。

### 8.7 Tier2 交付 ✅（完整）

**核心新增（两处原语，见 §5.5）**

- [x] `core/extensions/overlay.rs`（新）：`OverlaySize/OverlayInput/OverlayView/OverlayKey/OverlayEvent`
      + `overlay_view` / `overlay_event`（`ExtensionHook::Overlay` 门控）+ 4 个 inline 测试
- [x] `core/extensions.rs`：`Extension::{overlay_view, on_overlay_event, render_custom_message}`
      默认实现；导出新类型
- [x] `core/extensions/ui.rs`：`ShowOverlay` / `HideOverlay` / `CustomMessage` 请求 +
      `render_custom_message` 分派
- [x] `core/extensions/dock.rs`：`DockSpan::fallback: Cow` + `DockSpan::hex`（动态 badge 色）
- [x] TUI：`interactive/overlay.rs`（状态/滚动/输入/事件）、`handlers/overlay.rs`（按键与粘贴）、
      `render/overlay.rs`（标题/视窗/滚动条/底栏/输入行）、`render.rs` 布局接入（Inline 在输入框下方、
      Fullscreen 接管聊天区）、`handlers.rs` 键路由（覆盖层优先）、`handlers/mouse.rs` 滚轮命中、
      `interactive.rs` 处理 `ShowOverlay`/`HideOverlay`/`CustomMessage`、`app.rs` 字段与会话切换关闭

**扩展侧**

- [x] `subagent/config.rs`：新增 `join` 设置（smart/group/async + 窗口 ms）
- [x] `subagent/widget.rs`：`resolve_agent_color` 供 FleetView 复用
- [x] `subagent/notify.rs`：`transcript_rows`（转写行的主题键口径，与 `verbose_transcript` 同源）、
      `card_payload` / `render_card` / `CARD_TYPE` / `card_payloads_from_session`（会话 JSONL 扫描）
- [x] `subagent/manager.rs`：完成投递改为"`Continuation` + 卡片 + 落盘"；join 批处理
      （`batch`/`batch_generation` + 代际失效的窗口定时器）；`flush_batch`
- [x] `subagent/fleet.rs`（新）：FleetView 与查看器的内容/按键/输入语义；
      `remember_ctx`（缓存 `ToolExecCtx` + 运行时句柄，使 UI 能发起续跑）
- [x] `subagent.rs`：`hooks()` 增 `Overlay`；三个新钩子接线；`/agents` 菜单增 *Fleet view*；
      `on_session_switched` 重放卡片；`on_enabled_changed(false)`/会话切换清 UI 状态

**测试**（新增 20 个）

- [x] `core/extensions/overlay.rs` 4 个：门控（未声明 Overlay 的扩展不被轮询）、认领/不认领 id、
      事件消费语义、`Closed` 无主返回 false
- [x] `modes/interactive/overlay.rs` 4 个：Inline 行数口径、滚动夹取、follow/selected 跟随、
      提交与输入文本
- [x] `modes/interactive/handlers/overlay.rs` 3 个：键映射、滚动/Esc/Ctrl+C 默认语义、
      打字聚焦→退格→Esc 失焦→Enter 提交→粘贴
- [x] `modes/interactive/render/overlay.rs` 2 个：标题/视窗/底栏与区域记录、贴底与滚动窗口
- [x] `modes/interactive/handlers/mouse.rs` +1：滚轮命中覆盖层只滚覆盖层
- [x] `render/dock.rs` +1：`DockSpan::hex` 着色与空段默认前景
- [x] `subagent/fleet.rs` 4 个：Fleet 行选中标记与 badge 色、FleetView 空态/选中夹取、
      查看器状态/转写/输入标签、打开-关闭-未知目标路径
- [x] `subagent/notify.rs` +3：转写行主题键、卡片载荷预览上限与渲染一致、会话 JSONL 扫描过滤
- [x] `subagent/manager.rs` +1：join 两种模式（合并/逐个）
- [x] `subagent.rs` +1：卡片渲染分派 + 会话切换重放 + 不认领他人类型
- [x] `core/agent_session.rs`：后台完成用例改为轮询等待（通知在 `wait` 解消后入队）+ 断言三类请求

**验收**

```bash
cargo test          # 1155 passed（lib）+ 全部集成用例；0 failed
cargo clippy --all-targets   # 新增代码 0 warning（余 11 条均为既有）
```

### 8.8 Tier4 · S2 交付（进行中）

**已完成**

- [x] `config.rs`：新增 5 项设置（`tool_description` / `fallback_subagent` /
      `strict_agent_files` / `disable_default_agents` / `output_transcript`）+ 面板项 + 校验 + 落盘
- [x] `agent_types.rs`：`discover_with(cwd, DiscoveryOptions)`（strict → `errors`、disable_defaults
      → 无默认三类型）；新增 `find_any`（忽略 `enabled`，供 enable/disable/eject 使用）；
      解析 `disallowed_tools` / `isolated` / `output_transcript` / `run_in_background` / `inherit_context`
- [x] `prompt.rs`：`agent_tool_description_for(types, mode, cwd)`（full/compact/custom）+
      `render_custom_description`（3 个占位符，未知占位符原样保留并报告）
- [x] `agent_files.rs`（新）：`serialize_agent_file`（eject 产物可被加载器读回）+
      `disable_in_content` / `enable_in_content`（逐行、保留注释与键序、无法改写时如实拒绝）+
      `eject_path`（项目/全局 + 文件名安全化）/ `find_agent_file`
- [x] `output_file.rs`（新）：`.output` JSONL 运行转写（`encode_cwd` + 每用户根目录 + 追加写入）
- [x] `subagent.rs`：`tools()` 用配置化发现 + 描述模式（告警走既有去重）；
      `agent_tool` 严格模式拒绝派发、`fallback_subagent = none` 直接报错、frontmatter 锁定
      `run_in_background`/`inherit_context`；`/agents types|eject|enable|disable` 子命令
- [x] 核心 `SubAgentSpec`：新增 `isolated` / `disallowed_tools`，`materialize_sub_agent`
      在"收窄 + 递归防护"之后应用（isolated 通过 `registered()` 的扩展工具名剔除）
- [x] `manager.rs`：`output_transcript` 两条 spawn 路径建文件（排队分支在真正启动时才建）、
      路径写入 `AgentRecord.output_path`；卡片/查看器展示 `output`/`session` 路径

**测试（新增 12 个）**

- [x] `agent_files.rs` 4 个：逐行 disable/enable 保真与幂等、任意位置/大小写变体语义、
      eject 序列化往返（含 `tools: none|all`、禁用态、特殊字符加引号）、eject 路径作用域与安全化
- [x] `prompt.rs` +1：compact 更短、custom 缺失回退 + 占位符替换 + 未知占位符告警
- [x] `output_file.rs` 2 个：`encode_cwd` 约定、创建 + 追加 JSONL
- [x] `subagent.rs` +2：strict/fallback=none/frontmatter 锁定/disable_default_agents 四条路径、
      eject → disable → enable 文件往返（含"没有 .md 的类型"与未知类型）
- [x] `core/agent_session.rs` +1：`isolated` 剔掉扩展工具（白名单里的扩展工具同样被剔）、
      `disallowed_tools` 显式剔除
- [x] `manager.rs` +2：`.output` 落盘与路径记录、spec 字段透传

**验收**：`cargo test` → 1167 passed（lib）+ 集成全绿；`cargo clippy --all-targets` 新增代码 0 warning。

**S2 第二段（同轮完成）**

- [x] 核心 `SubAgentSpec`：新增 `extensions`（白名单）/ `exclude_extensions` / `clear_skills`；
      `materialize_sub_agent` 按「工具 → 所属扩展」映射过滤工具面（只在需要时查注册表）
- [x] `agent_types.rs`：`expand_ext_selectors`（`ext:<扩展>` / `ext:<扩展>/<工具>`）；
      `extensions:` 三态（`false`/列表/缺省）与 `skills:` 三态解析
- [x] `prompt.rs`：`preload_skills`（发现 → `/skill:` 展开 → `<preloaded_skills>` 段）
- [x] `manager.rs`：`build_spec` 增 `cwd` 参数，注入预载技能段；预载失败名一次性告警
- [x] `subagent.rs`：`isolated` 工具参数进 schema 并透传（frontmatter 优先）；
      `extensions: false` 等价 `isolated`；`ext:` 选择器在请求构造时展开

**S2 第三段（收尾完成：`@mention`）**

核心（契约见 §5.6）

- [x] `core/extensions/hooks.rs`：`ExtensionHook::UserPrompt`
- [x] `core/extensions.rs`：`UserPromptAction{Continue,Rewrite,Handled}`；`on_user_input` →
      `on_user_prompt`（门控）；新增 `on_exec_ctx`；`dispatch_user_prompt` / `dispatch_exec_ctx`
- [x] `core/agent_session.rs`：`Agent::exec_ctx()`；turn 开始 `dispatch_exec_ctx`
- [x] `src/main.rs`：会话建立后 `dispatch_exec_ctx`；`worker.rs`：会话切换后刷新
- [x] TUI `handlers.rs`：空闲与忙碌两条提交路径接入 `dispatch_user_prompt`；
      `steer.rs` 拆出 `queue_steer_text`（忙碌改写排队）
- [x] `plan_mode.rs` / `goal.rs`：迁到 `on_user_prompt`（`plan_mode` 声明 `UserPrompt`）

扩展

- [x] `subagent/mention.rs`（新）：`handle_base`/`assign_handle`/`parse_mention`/
      `strip_agent_prefix`/`describe_mention`/`resolve_handle_to_type`（保留 `main`）
- [x] `AgentRecord` 新增 `handle`/`alias`（共用命名空间，spawn 分配、淘汰回池、resume 保留）；
      `manager::resolve_mention` / `request_for_type`
- [x] `subagent.rs`：`on_user_prompt` 路由（`@main`/`@agent-`/steer/resume/类型 spawn、
      未知/裸句柄不吞输入），`on_exec_ctx` → `fleet::remember_ctx`，`hooks()` 按设置声明
      `UserPrompt`
- [x] `fleet.rs`：`cached_ctx` / `spawn_background_agent`（mention 与 `r` 续跑共用）；
      `reset()` 保留 ctx
- [x] `config.rs`：`agent_mentions: off|direct|model`（默认 `direct`，`model` 降级 direct + 告警）
- [x] `/agents` 列表行与 widget/结果卡显示 `@handle`

测试（新增 24 个，见 §8.3）

- [x] 核心：`UserPrompt` 门控/三态分发、`on_exec_ctx` 分发；handler：空闲认领/忙碌不排队/忙碌改写
- [x] `mention.rs` inline：逐条移植上游 `mention.test.ts`（句柄基础/编号保留/别名/精确解析/拒绝裸与路径）
- [x] `subagent.rs` inline：`@main` 改写、未知/裸句柄不认领、存活→steer、类型→spawn
- [x] `manager.rs` inline：handle/alias 分配与共享命名空间、`resolve_mention` 大小写、resume 保留 handle
- [x] 测试隔离修正：会话切换/禁用会 `reset_all`，相关 app/interactive/agent_types/commands 测试补 `subagent::test_lock`；
      UI 队列断言用例补 `AUTH_TEST_LOCK`

**验收**：`cargo test` → 1198 passed（lib，8 连跑稳定）+ 集成全绿（3 连跑）；
`cargo clippy --all-targets` 新增代码 0 warning。

### 8.9 S3 交付（Tier3 执行内核）

**已落地**（`src/extensions/subagent/workflow/`）

- [x] `Cargo.toml`：`rquickjs 0.12.2`（`futures`/`macro`/`parallel` + 默认类/属性/ArrayBuffer），仅需 C 编译器，离线可构建
- [x] `workflow/meta.rs`：`extract_meta` 字面量提取与校验（字符串/模板/注释/正则感知扫描器；
      拒绝模板插值；空格替换 `export ` 保持行号；隔离 rquickjs 上下文 + 100ms 中断上界）
- [x] `workflow/glue.js`：JS 侧全局 `agent`/`parallel`/`pipeline`/`phase`/`log`/`console`/
      `args`/`budget`；选项校验（`label`/`phase`/`model`/`agentType`/`effort`/`schema`；
      `gate`/`resume`/`isolation` 及未知项按**照上游口吻**报错）
- [x] `workflow/bridge.rs`：`AsyncRuntime` + `async_with` 驱动循环（**不 await JS Promise**——
      `PromiseFuture` 非 `Send`；改由脚本结束时调 `__done` + 宿主 await 通道，`Ctx` 本身 `Send`）；
      `__dispatch`/`__settle`/`__progress`/`__budgetSpent`；确定性前奏；
      并发 `max(1,min(16,cpus-2))`、每运行 1000 代理、每批 4096 项；中止/墙钟中断；
      致命错误（配额超限）经 `workflowFatal` 穿透 `parallel`/`pipeline`
- [x] `workflow/tool.rs`：`SubagentWorkflow` 工具（`script`/`scriptPath`/`args`；
      `name`/`resumeFromRunId` 明确报“未实现”；`agent()` → 前台 `manager::dispatch`）
- [x] `workflow/card.rs`：完成卡片（`CustomMessage` + `PersistSessionEntry`；
      `render_custom_message` 纯函数）；`details` 携带 agentCount/progress/meta
- [x] 核心：`SUBAGENT_TOOL_NAMES` 增 `SubagentWorkflow`（递归防护）
- [x] 测试 13 个：kernel（agent/parallel/pipeline/phase/log/args/budget、schema、
      确定性、错误折叠、配额、`meta` 扫描/拒绝）+ 工具端到端（假 runner）+ 卡片

**本切片内的有意近似 / 推到 S4**

- `agent({schema})`：S3 曾以"提示词要求 JSON + 宽松解析"近似，**S3b 已补上真工具注入**（见 §8.10），
  此处保留为历史记录。
- 工作流当前是**前台**（工具 await 完成并内联返回结果）；上游是后台任务（立即返回
  task ID + 完成通知）。后台语义 + 实时进度覆盖层待后续。
- `gate`/`resume`/`isolation`（worktree）与 journal/`resumeFromRunId`/具名工作流
  （`.prux/workflows/`）属 S4；工具层与 JS 层均**显式报错**，不静默忽略。

**验收**：`cargo test` → 1211 passed（lib，2 连跑稳定）+ 集成全绿；
`cargo clippy --all-targets` 新增代码 0 warning。

### 8.10 S3b 交付（结构化输出工具注入）

**已落地**

- [x] 核心（§5.7）：`InjectedChildTool` / `SubAgentSpec.injected_tools` /
      `ToolRebuildCtx.injected_tools`；`compose_tools` 无条件并入（含 snippet/guidelines）；
      同名告警；`ToolIndex` 收敛 6 处按名字解析（优先级 注入 > 内置 > 扩展）
- [x] kernel：`WorkflowInputs.validate_schema` 闸门（spawn 前、失败即 `fatal` 且不占配额）；
      agent 失败 settle 成 `null`（Q7 语义修正），失败原因另投一条 `workflow_log`
      （上游把它挂在 per-agent 进度条目上，那部分待下一刀；至少不静默）
- [x] `workflow/tool.rs`：建注入工具、删 `SCHEMA_SUFFIX` 与 `parse_json_lenient`、run 后用同一个
      编译好的校验器**定稿再校验一次**（上游 `applySchema` 的对应物）；`SubagentWorkflow` 的工具
      描述同步为“注入 `StructuredOutput` → 不匹配回推 → 从不交回则 `null`”
      （不承诺上游那句“补一次 prompt”）
- [x] `workflow/structured_output.rs`（新）：`StructuredOutput` 工具（照 `structured-output.ts`
      逐条移植：name/label/description/promptSnippet/guidelines/`parameters`/`prepare_arguments`）、
      `jsonschema 0.42.2` 深度校验（`$` 路径消息照上游口吻）、capture `{json, called, last_error}`
      且 last-valid-wins、编译三条检查（根 `type:"object"` / ≤64KB / 冒烟）
- [x] `Cargo.toml`：`jsonschema 0.42.2`（`default-features = false`，避开 `resolve-http/file`；
      依赖树已缓存，`cargo check --offline` 通过）

**有意偏离（本切片）**

1. **不做"补一次 prompt"重试**。上游在子代理完全没调工具时会再对同一 session 补一次
   `structuredRetryPrompt`，然后才判失败。prux 直接判 `null`（脚本侧契约相同：捕获到→对象，
   没捕获到→`null`）；差别只在成功率。忠实实现需要给 `manager` 加"run 结束后按条件再 prompt 一次"
   的钩子，而那个形状正是 S4 `resume` 语义的核心，不宜在 S3b 凭单一用户钉进 `SpawnRequest`。
2. **注入工具拿不到 provider 侧 strict 约束采样**（见 §5.7 末）：上游三重压力的第 1 重缺失，
   只剩"工具描述/guidelines 引导"+"校验失败 isError 回推"两重。
3. **核心参数校验仍是浅层**（`validate_tool_arguments`：顶层 `required` / 顶层类型 /
   `additionalProperties:false`）。`StructuredOutput` 的深度校验（嵌套 / `enum` / 数组 `items` /
   数值与字符串约束）在扩展侧用 `jsonschema` 完成，核心校验器不动。把核心校验整体换成真 JSON Schema
   是所有工具受益的独立切片，不在 S3b。
4. **工作流仍是前台**（工具 await 完成后内联返回），后台语义 + 实时进度覆盖层待补。

**验收**：`cargo test` 裸跑全绿（lib 1225 passed + 17 套件全绿）；
`cargo clippy --all-targets` 新增代码 0 warning。

**测试**（19 个新增）

| 层 | 位置 | 用例 |
|---|---|---|
| kernel | `workflow/bridge.rs` | 普通失败 → `null`（原因落在该 agent 进度条目的 `error` 上）；schema 闸门失败 → fatal 且跑到配额检查**之前** |
| 核心 | `core/agent_session.rs` | `ToolIndex` 优先级（注入 > 内置）；注入的 `prepare_arguments`/`execution_mode` 同源；注入工具在窄白名单 + `disallowed_tools` + `isolated` + 空 `extensions:` 下仍入表入提示，且不进 `get_all_tools()` |
| 扩展单元 | `workflow/structured_output.rs`（7） | 编译三检查（非对象根 / 超 64KB / 校验器走不动）；嵌套+`enum`+`items`+下界+`additionalProperties` 深度校验与 `$` 路径；last-valid-wins；`prepare_arguments` 救回；失败文案两种区分 |
| 端到端 | `core/agent_session.rs`（假 provider 演子代理调工具） | 子代理调 `StructuredOutput` → 脚本拿到**对象**；从不调工具（正文里那段形似合规的 JSON）→ 脚本拿到 **`null`** |

### 8.11 S3c 交付（工作流后台化 + 实时进度）

**已落地**

- [x] 工具后台化（§5.8）：解析/`meta` 校验 → 登记运行 → **立刻**返回 task id 与
      `details { taskId, workflow, status }`；描述与 `promptGuidelines` 对齐上游（含
      "runs in the background … do not poll or sleep waiting for it" 与 "Use `/agents workflows`"）
- [x] `workflow/task.rs`（新）：运行注册表（`wf_` + 12 hex、状态机 `running|completed|failed|killed`、
      `stop`、`reset_all`、`list`、`dock_lines`、`overlay_view`）；run 持有自己的取消标志，
      以它派生 `ToolExecCtx` 给子代理（父级 abort 与后台 run 解耦）
- [x] `workflow/progress.rs`（新，上游 `progress.ts` 的移植）：`parse_entries`/`collapse`/
      `display_state`/`build_phase_groups`（含 `meta.phases` 与 `phase()` 的模糊归并、
      未声明阶段自己成组、声明未跑的 `not-started`、无阶段时收成 "Agents"）/`stats`/
      `format_duration`/`gerund`/`footer_phase_label`/`header`
- [x] `workflow/notify.rs`（新）：`<workflow_result …>` 信封（done/total、tokens、
      tool_uses、duration、结果截断 4000）
- [x] `workflow/card.rs` 重写：卡片带上**阶段树**（每阶段 `✓/✗ done/total`）与**失败行**（含原因）；
      载荷纯 JSON，实时投递与会话重放同一条渲染路径
- [x] kernel：共享进度日志（`WorkflowInputs.progress`，`progress_sink` 删除）、
      `workflow_agent` 条目（start/progress/done 三次跃迁、跨跃迁累积）、`AgentReport`
      （失败也带元数据）、`index` 与配额计数器合一且先查后加、`budget.spent()` 用 output token
- [x] `manager`：`BatchItem::Agent | Rendered` 的中性 join 批次（工作流完成与代理完成合并成
      一条 follow-up）；`notify::batch_envelope` 改为接受已渲染的信封
- [x] 核心：`Extension::wants_redraw()` + `extensions::wants_redraw()` + 交互 80ms tick 置脏
- [x] `subagent`：dock 两段组合、`/agents workflows` 子命令与 `OverlayKind::Workflows`（只读）、
      `/agents stop wf_…` 分叉、卡片重放扩到两种 `custom_type`、`reset_all` 一并清空
- [x] 测试 26 个新增（kernel 进度条目/共享日志、`progress.rs` 十个纯模型、`task` 生命周期与
      渲染、卡片含失败行、通知信封三种状态、中性批次合并、工具立刻返回 + 后台完成通知 + 卡片、
      dock/重绘、只读覆盖层、`/agents stop` 路由；两个 S3b 端到端用例改为直接驱动工具）

**有意偏离（本切片）**

1. **运行期间的行没有 token/recordId**：它们只在结算那条条目上。上游靠 manager 的
   `onSpawned` 回调 + 每次用量事件做到实时更新；prux 补它需要给 `SpawnRequest` 加回调
   并在 kernel 里轮询活用量，而它的两个消费者（检查器打开子会话、行内实时用量）都在 S4。
2. **workflow 的子代理占前台并发额度**：它们相对 run 是前台，所以
   `foreground_max_concurrent > 0` 时会与 run 自己的 `min(16, cpus-2)` 叠加（默认 0 = 不限，
   无影响）。上游给 run 的子代理打 `workflowId` 把它们从会话池里排除。
3. ~~**脚本不自动落盘**~~：**S4a 已收掉**（§8.12）：会话键经 `on_session_start` /
   `on_session_switched` 缓存，脚本落在会话任务目录并返回 `Script: <path>`，同时 `.output`
   转写的会话子目录偏离也一并修正。
4. **不提供双栏检查器**（pause/skip/retry、第二栏显示脚本/对话）：属 S4。本切片的覆盖层
   是只读的；`task::WorkflowRun` 也**不**保留脚本源与 `args`（不放假字段），S4 需要时再加。
5. **`gate`/`resume`/`isolation`、journal、具名工作流**仍显式报错（S4），与本切片无关。

**验收**：`cargo test` 裸跑全绿（lib 1246 passed，17 套件共 1385，2 连跑一致）；
`cargo clippy --all-targets` 新增代码 0 warning。

### 8.12 S4a 交付（gate / 具名工作流 / 脚本落盘 / collisions 让位）

**已落地**

- [x] `agent({ gate })`（§5.9）：glue 选项表加入 `gate` → `AgentRequest.gate` → `run_one_agent`
  在子代理**成功之后**跑 `sh -c`；非零退出/超时 → 该 agent 失败、命令输出即错误 → 脚本得 `null`；
  10 分钟超时（上游同值），超时消息明确说超时
- [x] `workflow/saved.rs`（新）：两层根 + 项目信任门控 + first-hit-wins；名字校验
  （`^[a-zA-Z0-9][a-zA-Z0-9._-]*$`、≤128）+ **拒绝符号链接**；只有带 `meta` 声明的 `.js` 算工作流；
  `list()` 供错误消息与列表用
- [x] **优先级修正**：`scriptPath` > `script` > `name`（此前 `script` 赢了）；`name` 命中时把该文件
  记为运行的 `scriptPath`
- [x] 脚本自动落盘（§5.9）：`<tmp>/prux-subagents-<uid>/<encoded-cwd>/<session>/tasks/<runId>.workflow.js`，
  正文返回 `Script: <path>` + "To iterate…"；`subagent/session.rs`（新）缓存会话键
- [x] 顺手收掉既有偏离：`.output` 转写现在也按会话分子目录（`output_file.rs` 的"拿不到父会话 id"
  注释作废）
- [x] `collisions` 让位（§5.9）：`workflows: auto|on|off` 配置键（面板第 14 项）；
  `Extension::filter_extension_tools` 收起 `SubagentWorkflow`；`/agents workflows` 在让位/关闭时
  改为报说明；`session::verdict()` 惰性缓存 + 在注册/会话/启用/设置变更时失效
- [x] 工具 schema 与描述同步：`name` 不再说 "not yet supported"，`scriptPath` 说明它优先于 `script`，
  描述里补上"每次调用都会落盘 + 用 `scriptPath` 迭代 + 具名工作流放哪"
- [x] 测试 15 个新增（saved 解析/信任门控/优先级、`session` 的键与判定、gate 通过/失败端到端、
  具名工作流端到端 + 落盘路径、让位/`on`/`off` 三态、output_file 的会话层与穿越防护）

**有意偏离（本切片）**

1. **根只有两层**（`.prux/workflows/` + `<agent_dir()>/extensions/workflows/`），没有上游共享的
   `.agents/workflows/` 那一层——沿用 §3.2 "发现路径收敛为两层"，不为工作流另立第三套。
2. **`workflows` 是三态枚举而非上游的 `bool + pinned`**：prux 的配置缺键即默认，
   无从表达"用户显式设过 true"，所以把三种行为直接序列化（§5.9 有完整理由）。
3. **`resume` 与 `resumeFromRunId` 仍未实现**（S4b）：`agent({ resume })` 与
   `resumeFromRunId` 都显式报错，不静默忽略。
4. **`--subagents-workflow-file`（上游唯一带值的 CLI flag）未迁移**：需要给核心
   `CliFlagDef` 加"带值 flag"的支持，属 S4b/独立小刀。

**验收**：`cargo test` 裸跑全绿（lib 1256 passed，17 套件共 1395，2 连跑一致）；
`cargo clippy --all-targets` 新增代码 0 warning。

### 8.13 S4b 交付（journal 重放 + `agent({ resume })`）

**已落地**

- [x] `workflow/journal.rs`（新，§5.10）：键、容错读取、append、`resumed` 整盘作废
- [x] 桥：`WorkflowInputs.journal`；`replayAt` 语义（位置 + 键 + 上次成功，任何不符**永久**结束前缀）；
  重放条目 `cached: true`/`durationMs: 0`；重放文本再过一遍 schema（`SchemaHooks::check_text`）；
  结算即 append；`WorkflowOutcome.budget` 依旧只累加真跑的 output token
- [x] `SchemaGate` 拆成 `SchemaHooks { compile, check_text }`（一个接缝，两件能力）
- [x] `resumeFromRunId`（§5.10）：同会话 + 已终结 + 有 journal；本次没给来源时用上一次的脚本；
  `resumed_from` 与摘要里的"N of M replayed"
- [x] `agent({ resume })`：glue 的互斥校验 + runner 的 `label → 记录 id` 表 + `dispatch` 的 resume 路径
- [x] 顺手修正计数口径：`progress::stats` **先折叠再数**（日志是 append-only 的，
  一个 agent 有 start/progress/done 多条，按条目数会把 3 个 agent 数成 9 个；
  上游 `updateWorkflowProgressBatch` 也是按折叠后的 agent 计数）
- [x] 工具 schema/描述：`resumeFromRunId` 不再写 "not yet supported"，`agent()` 的选项表加上
  `gate`/`resume` 与它们的语义
- [x] 测试 12 个新增（journal 的键/容错/整盘作废、`check_text`、重放端到端（改一句只跑一个 agent
  + 摘要文案 + `cached` 计数）、未知 run id、`agent({ resume })` 的未知 label；
  **真 resume 的端到端**（真 `make_sub_agent` + 假 provider：两次调用落在同一个 record id、
  拿到续跑后的新答案）；`stats` 的折叠回归）

**有意偏离（本切片）**

1. **journal 键少一位 `isolation`**：prux 还没有 worktree 隔离（S6）。加它会让正则形式整体位移、
   磁盘上已有 journal 全部作废（多付一次运行）；真到那时也可像 schema 那样按条件追加来避免。
2. ~~**`--subagents-workflow-file`（上游唯一带值的 CLI flag）未迁移**~~：**S4d 已收掉**
   （§5.12 / §8.15）——核心加了带值 flag 通道，扩展侧推迟到拿到可物化 ctx 时启动。
3. ~~**双栏检查器与 pause/skip/retry**~~：**S4c 已落地**（§5.11 / §8.14）——
   覆盖层宽度接缝 + kernel 控制面（每个活跃调用一个取消标志）。

**验收**：`cargo test` 裸跑全绿（lib 1264 passed，17 套件共 1403，6 连跑一致）；
`cargo clippy --all-targets` 新增代码 0 warning。

### 8.14 S4c 交付（运行检查器 + pause/skip/retry）

**已落地**

- [x] kernel 控制面（§5.11）：`WorkflowControl` + `control_sink`；每个活跃调用一个取消标志，
      经 `AgentRequest.abort` 由 runner 用作 child 的 `parent_abort`；暂停等待与唤醒；
      skip/retry 的意图、重跑循环、`attempt`/`lastAttemptReason` 记入条目；
      掐 run 时置位全部活跃调用
- [x] `workflow/inspector.rs`（新）：两栏渲染（左树右详情）、阶段/扁平两视图、选中行导航、
      详情栏（状态/类型/记录 id/用量/错误/预览）、底部"此刻真能用"的键位提示
- [x] 核心：`Extension::overlay_view(id, cols)`（宽度接缝；TUI 在渲染期传真实宽度）
- [x] `fleet`：新增 `OverlayKind::Inspector`；运行列表可导航（↑↓ + Enter 进检查器）；
      检查器按键路由（`p`/`s`/`r`/`x`/`f`/`c`）与 `RunStatus::Paused` 的展示
- [x] `task`：`control` 字段 + `set_control` + `set_paused`；`overlay_view(selected)`
- [x] 测试 16 个新增（kernel：暂停拦启动与 resume 放行、skip 的 null + skipped + journal 记失败、
      retry 重跑同一次调用并记 attempt、结算后 skip/retry 是 no-op；检查器：两栏布局/选中详情/
      扁平与阶段、行与选中目标的推导；fleet：列表导航 + Enter 进检查器 + 检查器按键）

**有意偏离（本切片）**

1. **两栏由扩展按宽度自己拼**（核心只是"把宽度告诉扩展"），而不是给 `OverlayView` 加真正的
   分栏原语：上游是 `pi` 的 overlay API 自带宽度，prux 的覆盖层是一列行——把宽度透传下来是
   最小且诚实的接缝，加一套布局原语则是另一个量级的事（且只有这一个消费者）。
2. **`--subagents-workflow-file` 仍未迁移**（同 §8.13 偏离 2）：需要核心的"带值 CLI flag"
   与会话启动即跑的路径。
3. **`f` 只有两级（阶段 / 扁平）**，没有上游的过滤器（`f` 在上游是 filter+视图切换）；
   行的排序也不支持重排。
4. **检查器不做跨运行导航**：它一次看一个运行（Esc 回列表再选），与上游"从 fleet 行直接进
   单次运行"的入口一致。

**验收**：`cargo test` 裸跑全绿（lib 1270 passed，17 套件共 1409）；`cargo clippy --all-targets`
新增代码 0 warning。

### 8.15 S4d 交付（`--subagents-workflow-file`）

**已落地**

- [x] 核心（§5.12）：`CliFlagDef.takes_value` + `Extension::apply_cli_flag_value` +
      `ParsedArgs.extension_flag_values` + main 分发；clap 侧 `Set`/`num_args(1)`
- [x] `plan_mode` 的字面量补 `takes_value: false`（既有 bool 语义不变）
- [x] `subagent`：声明 `--subagents-workflow-file`；记下路径 → 首次 `on_exec_ctx` 时启动
      （无运行时句柄则留待下次；`off`/让位时清掉）；启动走 `execute_file` → `execute`
- [x] 测试 3 个新增（clap 的 bool/带值/缺值报错；扩展侧 flag→启动→待跑项取走→`off` 不启动）

**有意偏离**：见 §5.12 末（flag 只在扩展已启用时注入）。

**验收**：`cargo test` 裸跑全绿（lib 1272 passed，17 套件共 1411，3 连跑一致）；
`cargo clippy --all-targets` 新增代码 0 warning。

### 8.16 S5a/S5b 交付（配置双层合并 / 型号解析与 scope）

**已落地**

- [x] 配置双层逐键合并（§5.13）：`apply_layer` + `project_config_path`（信任门控）+
      `ConfigKey`（两层 mtime 缓存）；面板仍写全局层
- [x] `model.rs`（新）：fuzzy 解析（精确/打分/提供方回退/可读错误）+ scope 判定
      （复用核心 `read_enabled_models` + `resolve_model_scope`）
- [x] `SpawnRequest.model_source`：工具层决定型号来源（caller > frontmatter > 继承），
      `dispatch` 里先解析再判 scope；拒绝/告警两条路径
- [x] 新设置 `scope_models`（缺省关）+ 面板项
- [x] 测试 12 个新增（双层合并的逐键/门控/缓存失效；fuzzy 的精确/dot-dash/子串/日期戳/
      提供方回退/找不到；scope 的三态与 `dispatch` 端到端拒绝+放行）

**有意偏离（本切片）**

1. **`scope_models` 不检查"继承父级"那一路**：扩展在 spawn 时看不到父级模型
   （`ToolExecCtx` 不带模型）。上游那条路只告警，影响面小；要收敛得给 ctx 加一个字段。
2. **面板只写全局层**（上游面板可写项目级）：prux 的扩展设置面板没有"写到哪一层"的概念，
   加它要动面板协议；手写项目文件已经能用。
3. **可用型号目录进程内缓存一次**：换凭据/加提供方要重启进程（上游每次解析）。
4. **frontmatter 型号解析失败不退化为继承**（§3.3）。

**验收**：`cargo test` 裸跑全绿（lib 1279 passed，17 套件共 1418，3 连跑一致）；
`cargo clippy --all-targets` 新增代码 0 warning。

### 8.17 S5c 交付（定时子代理）

**已落地**

- [x] `schedule/parse.rs`：四种调度串的判定 + 6 段 cron 解析（`*`/`a`/`a-b`/`*/n`/`a-b/n`/列表）
      + **本地时间**的 `next_run`（DOM/DOW 取或、DST 取最早、4 年上限、无解返回 None）
- [x] `schedule/mod.rs`：会话级 store（原子写）、`add`/`cancel`/`list`/`next_run`、1 秒 tick、
      到点走 `manager::dispatch`（后台 + `bypass_queue`）、记账（running/success/error/runCount/
      nextRun）、过期一次性任务标记 + 关停 + 告警
- [x] `SpawnRequest.bypass_queue`：不占后台额度也不排队（定时任务专用）
- [x] `Agent` 的 `schedule` 参数（启用时才进 schema）+ 组合守卫 + 类型在创建时校验 +
      提示词里的 `scheduleGuideline`
- [x] `/agents schedules` 列表覆盖层（↑↓ + `d` 取消 + 详情）+ 子命令声明；设置项 `schedule`（默认开）
- [x] 测试 13 个新增（判定顺序/错误例子、cron 字段与周日常量、本地时间 next_run 与"永远不来"、
      三种 kind 的下一次、store round-trip 与会话重绑、重名/非法、到点真派发并记账、
      类型解析失败记 error 并告警、schema 门控）

**有意偏离（本切片）**

1. **store 放在 agent 目录**（`<agent_dir>/extensions/schedules/<session>.json`）而不是项目里
   （上游 `<cwd>/.pi/subagent-schedules/`）：prux 的会话状态一律在 agent 目录，
   往项目里随手写状态文件不是这里的约定。
2. **没有 PID 锁**：单进程，不需要；要跨进程编辑同一条目自然会撞上 read-modify-write，
   真出现多进程需求时再加锁。
3. **扫描是 1 秒定频**（上游每任务一个 `setInterval`/`setTimeout`）：判断集中在一处，
   会话切换/取消/一次性关停都不会漏；秒级精度与上游实际精度一致。
4. **菜单只有"看 + 取消"**，与上游 v1 相同（创建的正路是 `Agent` 的 `schedule` 参数）。
5. **`--subagents-schedule-*` 之类的 CLI flag 未迁移**（上游也没有）。

**验收**：`cargo test` 裸跑全绿（lib 1288 passed，17 套件共 1427，3 连跑一致）；
`cargo clippy --all-targets` 新增代码 0 warning。

### 8.18 S6a 交付（git worktree 隔离）

**已落地**

- [x] `worktree.rs`（新）：create / cleanup / prune，全部走带超时的异步 git
      （子目录层级保留、严格失败、分支名撞了换一个、结果里点名分支）
- [x] `Agent` 的 `isolation` 参数（受 `worktree_isolation` 门控）+ frontmatter `isolation`
      （从 `UNSUPPORTED_FIELDS` 移出，成为真支持）+ 提示词里的 isolation 说明
- [x] 设置 `worktree_isolation`（缺省开）+ 面板项
- [x] manager：后台/前台两条路径都在**跑之前**建副本（失败即记 error）、
      副本目录写进 `spec.cwd`、跑完收尾并把 `worktree_result` 记进记录
- [x] `SpawnRequest.isolation` + `AgentRecord.worktree`/`worktree_result`；会话开始 prune 孤儿
- [x] 测试 5 个新增（真 git：往返提交+删副本、非仓库严格失败、子目录层级保留；
      manager 端到端：副本里跑 → 改动提交到分支并写进结果 → 非仓库记 error →
      开关关掉丢弃请求）

**有意偏离（本切片）**

1. **副本路径带 pid**（`<tmp>/prux-agent-<id>-<pid>`）而不是随机 uuid：prux 没有 uuid 依赖，
   而 id + pid 已经不会撞（残留目录会让 `worktree add` 失败，由 §5.15 的严格失败兜住）。
2. **没有给 worktree 隔离做"运行中可见"的 UI 行**：结果正文与记录里有分支名，
   检查器/卡片还没把它单独渲染出来。
3. **`isolation: "remote"` 不迁移**（上游是 Claude Code 的另一种模式，pi 自己也没实现）。

**验收**：`cargo test` 裸跑全绿（lib 1292 passed，17 套件共 1431，3 连跑一致）；
`cargo clippy --all-targets` 新增代码 0 warning。

### 8.19 S6b 交付（嵌套子代理）

**已落地**

- [x] 核心：`SubAgentSpec.agent_id/depth` → `ToolRebuildCtx` → `ToolExecCtx`（扩展据此认身份）；
      递归防护放行"显式写进 `spec.tools` 的编排工具"
- [x] `nested.rs`（新）+ 设置 `max_subagent_depth`（缺省 2，面板项）
- [x] `build_spec` 在深度允许时把三个编排工具写进 child 白名单（上限作为参数传入）；
      `AgentRecord.parent_agent_id/depth`；`caller_of` / `owned_record` / `abort_owned_children`
- [x] `Agent` 工具：嵌套默认前台、身份与深度入请求；到顶时 handler 明确拒绝（防 `tools: Agent`
      绕过上限）；`get_subagent_result`/`steer_subagent` 走属主作用域
- [x] dock 缩进显示嵌套层级
- [x] 测试 6 个新增（核心：放行显式白名单、**端到端** 主→子→孙三层并断言父子关系与结果；
      扩展：深度上限的 build_spec 行为、身份与属主作用域、到顶拒绝）
- [x] 顺手修掉两处**测试隔离**问题（都是老的全局态假设）：
      配置缓存加了测试专用的"钉住"（持锁期间别的线程的重载被丢弃——`agent_dir` 是线程本地
      override，键必然"变化"，一重载就把当前用例的配置清成缺省）；
      集成测试里对进程级注册表的断言（dock/`wants_redraw`）删掉，改由持锁的单测覆盖；
      cron 单测的整数下溢改成 `saturating_sub`

**有意偏离（本切片）**

1. **`allowed_subagents` v1（S6b 时）**：当时仅解析+限型，**不**作为嵌套开关（见 §11.5 H1）；
   S7b 已改为 opt-in 开关。
2. **嵌套工具与顶层工具共用同一份 schema/描述**：上游给嵌套工具一份更窄的 schema
   （"child-safe"）并单独措辞。prux 靠 handler 的作用域与限深保证安全，schema 不分裂。
3. **没有 `awaitStartup` 那类"启动完成"信号**：物化仍在本层同步完成（见 8.18 的同类说明）。

**验收**：`cargo test` 裸跑全绿（lib 1295 passed，17 套件共 1434，15 连跑 lib + 4 连跑全套一致）；
`cargo clippy --all-targets` 新增代码 0 warning。

### 8.20 S6c 交付（跨扩展事件总线 + RPC）

**已落地**

- [x] 核心（§5.17）：`ExtensionHook::ExtensionEvent` + `Extension::on_extension_event`；
      `core/extensions/events.rs::{emit(from,name,payload), dispatch}`；生命周期门控 +
      发送方名校验；**重入队列化**（当前 dispatch 返回后 FIFO drain，`MAX_CHAIN=1024`）；
      非 handler 上下文立即投递；payload=`Value`、不按会话隔离。
- [x] `subagent/events.rs`：发 `subagents:{ready,created,started,completed,failed,steered,compacted,
      scheduled,scheduler_ready}`（顶层才发生命周期事件）；RPC 四方法 `ping/spawn/stop/consume`，
      回执通道 `<channel>:reply:<requestId>`，信封 `{success,data?}`/`{success,error}`，
      `PROTOCOL_VERSION=2`；`spawn` 顶层后台 + `options.model` fuzzy 解析与 scope；
      `stop` 仅顶层（`isTopLevelAgent`）；`consume` 抑制完成通知。
- [x] `AgentRecord` 新增 `workflow_owned`/`consumed`/`compaction_count`；`consume_result`；
      `get_subagent_result` 同步具备 consume 语义；前台补 `mark_running`（顺带修正前台一直显示 queued）。
- [x] `on_extension_event` 为同步接口；`spawn` 在 handler 里 `tokio::spawn` 完成后回执。
- [x] 测试 11 个新增（核心总线 5 + 扩展：生命周期投递 / ping / 无会话 / spawn 回 id /
      stop 拒嵌套 / consume 标读）。

**有意偏离**：`subagents:compacted` 的 `reason`/`tokensBefore` 取自 child 事件流；
`subagents:ready` 在会话开始时也重发一次（扩展注册早于其它扩展时不会漏）。

### 8.21 S6d 交付（mention clone / `agent_mentions=model`）

**已落地**

- [x] 核心（§5.18）：`SubAgentSpec.context_messages`（显式实时对话，覆盖 `inherit_context`）；
      `Extension::on_user_prompt_with_context(text, messages, busy)`（默认桥接旧 `on_user_prompt`）+ dispatch 透传。
- [x] `mention_clone`：克隆当前对话 → 隐藏回合（只带 `Agent`、`context_messages` 为实时消息）→
      副本拿 `message + <system-reminder>` 写 prompt → 顶层后台 spawn（marker 身份 + 一次 spawn 守卫）；
      失败回落 direct 并告警；`agent_mentions` 默认收敛回 `model`。

**有意偏离**：clone 只带 `Agent` 一种工具（其余全剔）；副本不落盘、不登记（无 widget/fleet 行）；
副本发起的前台 spawn 被强制改为后台（前台结果会随副本被丢弃）。

### 8.22 S6d 交付（allowed_subagents / reportUsage / showModel / Esc-wait / @ 补全 / 工作流行）

**已落地**

- [x] `allowed_subagents`：`AgentType` 解析 + `Agent` handler 按调用者 canonical 名限型（别名解析）。
- [x] `reportUsage`：配置 `report_usage`（默认 off）+ usage 池（sink 门控）+ `AfterToolCall`
      把池挂到下一个真实工具结果（核心本就计入 `toolResult` 的 usage）。
- [x] `showModel`：核心 `ToolExecCtx.parent_model` + `AgentRecord.model`（显式解析 or 继承父级）+ 设置 `show_model`。
- [x] Esc 只取消等待：`result_tool` 的 `wait` 与 `ctx.parent_abort` `select`；中止返回运行中快照。
- [x] `@` 补全：核心 `ExtensionHook::Suggestions` + `suggestion_candidates`；`suggest.rs` 并入 `@handle`/`@type`。
- [x] 工作流行：FleetView 合并 live 工作流运行（Enter 进检查器、`s` 停）。

### 8.23 S6d 交付（持久记忆）

**已落地**

- [x] `memory.rs`：三档路径（user=`<agent_dir>/agent-memory/…`，project/local=`.prux/…`）、
      信任门控（project/local 读写都要项目信任）、非法名/symlink 拒绝、`MEMORY.md` 200 行截断、
      读写 vs 只读分支（看添加记忆工具前的有效工具集）。
- [x] `AgentType.memory` 解析（`memory` 移出 `UNSUPPORTED_FIELDS`，现已为空）；
      `build_spec` 注入 `append_system_prompt` 并补 read/write/edit，失败只告警不阻断派发。
- [x] 测试 6 个新增（名字/读写/只读/信任/截断 + `build_spec` 两分支 + frontmatter 解析）。

### 8.24 Tier4 收尾：测试隔离修正

本轮新增用例暴露了三处**进程级全局态**的既有测试隔离缺口，一并修正：

1. `core/extensions/footer.rs::dispatch_reaches_all_extensions` 的 `Spy` 注册后无法注销，
   且在 handler 里断言“事件类型必须是 x”，会把并行的 goal 事件误判为失败——改为记录后再断言。
2. `core/oauth/openai_codex` 两个 browser-flow 用例会争抢 1455 回调端口——加 `AUTH_TEST_LOCK` 串行。
3. `core/agent_session` 的子代理用例与 `extensions/subagent` 用例统一 `AUTH_TEST_LOCK → manager::test_lock()`
   锁序（前者原本只持 test_lock，与计划/目标用例争全局 UI 队列）。

**验收**：`cargo test --offline` 裸跑全绿（lib 1320 + 17 套件），12 连跑 lib 一致；
`cargo clippy --all-targets` 新增代码 0 warning（仍 11 条既有）。

### 8.25 Tier2 残余：查看器滚动键接用户 keybindings（row 82）

**已落地**

- [x] `handlers/overlay.rs`：`scroll_binding` 把 `tui.select.{up,down,pageUp,pageDown}` 的
      用户绑定解析为 `OverlayKey`（默认 ↑↓/PgUp/PgDn 不变）；扩展侧接口不变。
- [x] 测试：默认绑定 + 自定义 `ctrl+u`/`ctrl+f` 映射。

顺带把 §2 里四行**陈旧状态**回填为已交付（S3/S3b）：86（沙箱全局）、87（`meta` 校验）、
88（确定性前奏）、89（并发/配额上限）。

**验收**：`cargo test` 全绿；`cargo clippy` 新代码 0 warning。

### 8.26 `/agents` 一级菜单补齐调度/工作流入口（row 45）

- [x] 一级菜单新增 `schedules  Scheduled jobs (N total)`（`schedule` 开关开启时）与
      `workflows  Workflows (N runs)`（未让位时）；两个入口与子命令共用
      `open_schedules_or_warn` / `open_workflows_or_warn`（同一条告警文案）。
- [x] 陈旧状态回填：row 40（`isolated`/`isolation`）、row 47（上游只一个 CLI flag）、
      row 73（skills 预载）均为已交付。
- [x] 创建向导的手动路径：由 §8.27 补上（`wizard.rs`）。
- [x] 向导的 "Generate with Claude" 生成路径：汇总步 `g` 把生成提示交给主模型写盘（S7）。
- [ ] 仍缺：手动路径的系统提示正文只能是单行（覆盖层无多行 editor 原语）——生成路径不受此限。

**验收**：`cargo test` 全绿；`cargo clippy` 新代码 0 warning。

### 8.27 `/agents → Create new agent` 手动向导（row 31/45 收尾）

- [x] `wizard.rs` + `fleet` 的 `OverlayKind::Wizard`：多步状态机（Location/Name/Description/
      Tools/Model/Thinking/Review），自持 `Char`/`Backspace`/`↑↓`/`Enter`/`Esc`（不依赖 TUI
      内联输入的 focus 边沿）；最后 `eject_path` + `serialize_agent_file` 写 `<target>/<name>.md`，
      不覆盖已存在文件；project 目标要求项目受信任。
- [x] 一级菜单新增 `create  Create new agent`。
- [x] "Generate with Claude" 生成路径（S7 G6）与系统提示步（S7d J3，多行编辑器）。
- [x] 测试：名字校验 / 步骤推进与回退 / 写盘可被重新发现 / project 信任门控 / 系统提示步编辑器与正文回退。

**验收**：`cargo test` 全绿（lib 1325）；`cargo clippy` 新代码 0 warning。

### 8.28 S7d：覆盖层多行编辑器 + 类型 Edit + 向导系统提示 + 工作流边界

- [x] 核心原语：`OverlayEditor`（`OverlayView.editor`）——扩展给标签/初值/可见行数与 `generation`，
      核心持编辑缓冲（复用 `editor::Editor`）与光标、按宽度软折行渲染；`OverlayKey` 补
      `Left/Right/Delete`，新增 `OverlayEvent::EditorSubmit`（Ctrl+S 回传整段文本）。
      generation 变化才重新装载，同一 generation 的每帧拉取不打断用户编辑。
- [x] 类型 Edit：二级菜单 `edit` + `/agents edit <type>`；Fullscreen 编辑整个 `.md`，Ctrl+S 整文件写回。
- [x] 向导系统提示：新增 `System prompt` 步（多行）；`editor_submit` 存正文并进汇总，空则回退
      Description；Generate 路径把正文一并交给模型。
- [x] 工作流 `checkBoundary`：`glue.js` 校验循环/非有限数/undefined/BigInt/符号/函数/符号键/
      稀疏数组/非纯对象，覆盖 schema、嵌套 `workflow()` 的 args 与返回值；args 与返回值加
      512KiB 体积上界（顶层 args 在启动前、结果在交付前）。
- [x] 测试：覆盖层编辑器装载/编辑/Ctrl+S/Esc、`outer_rows` 占位、渲染保底栏、向导系统提示步、
      `edit_type` 读→改→写、边界拒绝与体积上界（含嵌套）。

**验收**：`cargo test` 全绿（lib 1356）；`cargo clippy --all-targets` 0 warning。

### 8.6 文档 ✅

- [x] `migration-subagents.md` 第 0–8 章（契约冻结 + 实施回填）
- [x] 第 2 章状态列回填（Tier1 行 ✅ / 偏离行 ⚠️ / 推迟行 🔶）
- [x] 第 5.4 节：实施中的 4 处契约修正 + 1 处能力补强
- [x] 第 7.4 节：原"粗糙点"逐条结算为实际结论
- [x] 第 8 章：核心/扩展/测试/文档的完整交付清单
- [x] 第 8.7 节：Tier2 切片 1 交付清单（含验收命令）
- [x] 第 7.2 节改写为"剩余 Tier2 项的落地形态（待决策）"：给出覆盖层原语建议方案与
      三个未决点

---

## 九、后续切片路线（已与用户确认）

保真度策略：**契约照上游**（工具名/参数/状态词/frontmatter 字段/事件名），prux 缺基建的项做
**最小但真实**的核心接缝（真功能，不放假实现、不留 TODO 空壳）；与既有偏离冲突时按上游语义
收敛并回写第 3 章。

| 片 | 内容 | 状态 |
|---|---|---|
| **S1** | Tier2 收尾：覆盖层原语 → FleetView、会话查看器、完成卡片、join 分组 | ✅ 已完成（§8.7） |
| **S2** | Tier4 无新基建项（见 §8.8） | ✅ 完成（新增核心 `UserPrompt`/`on_exec_ctx` 接缝与 `@mention` 路由；顺手修正一批测试串行依赖） |
| **S3** | Tier3 执行内核：rquickjs 接线 + 沙箱前奏 + `agent/parallel/pipeline/phase/log/args/budget/schema` + `meta` 校验 + 并发/配额上限 + 进度事件 + `SubagentWorkflow` 工具 + 进度卡 | ✅ 完成（§8.9 内核/工具/卡片；其后由 S3b/S3c 补齐结构化输出与后台语义） |
| **S3b** | 结构化输出工具注入：核心 per-child 注入接缝 + `StructuredOutput` 工具 + `jsonschema` 深度校验 + capture 回传 + agent 失败=`null` | ✅ 完成（§5.7 / §8.10） |
| **S3c** | 工作流后台化 + 实时进度：运行注册表、`progress.ts` 移植、`workflow_agent` 条目、dock/运行列表覆盖层、完成通知并入 join 批次、`wants_redraw` | ✅ 完成（§5.8 / §8.11）——S3 遗留项已清 |
| **S4a** | `gate`、具名工作流（两层根 + 信任门控 + `meta` 过滤）、脚本自动落盘、`collisions` 让位、`scriptPath`/`script`/`name` 优先级修正 | ✅ 完成（§5.9 / §8.12） |
| **S4b** | journal 重放（`resumeFromRunId`）、`agent({ resume })` | ✅ 完成（§5.10 / §8.13） |
| **S4c** | 双栏检查器 + pause/skip/retry | ✅ 完成（§5.11 / §8.14） |
| **S4d** | `--subagents-workflow-file`（核心带值 CLI flag） | ✅ 完成（§5.12 / §8.15） |
| **S5a/S5b** | 插件私有配置双层合并、model scope + fuzzy 型号解析 | ✅ 完成（§5.13 / §8.16） |
| **S5c** | schedule（cron / `+10m` / interval + 存储 + 菜单） | ✅ 完成（§5.14 / §8.17） |
| **S6** | Tier4 基建二：跨扩展事件总线 + RPC、git worktree 隔离与完成提交、nested 子代理（`allowed_subagents` + 属主作用域委派工具 + 深度 2）、mention-clone | ✅ 完成（S6a/S6b/S6c/S6d，§8.18–§8.24）|
| **S7** | 剩余项清零：工作流 `isolation` / 嵌套 `workflow()` / 未 await 检测；设置 `fleetView`/`rememberAgents`/`viewerMarkdown`/`defaultMaxTurns`/`graceTurns`；向导 Generate with Claude；join 批量合计 | ✅ 完成（§11.1） |
| **S7b** | 复审补齐：嵌套 opt-in（`allowed_subagents`）、eject 序列化补字段、自定义描述上游占位符、工作流失败子代理→`null` | ✅ 完成（§11.5） |
| **S7c** | 全量复审补齐：沙箱禁用 `eval`/`Function`、类型管理 `delete`/`reset` + 交互二级菜单、嵌套 token 上卷 | ✅ 完成（§11.7） |
| **S7d** | 剩余两项：覆盖层多行编辑器原语 + 类型 `Edit` + 向导 `System prompt` 步；工作流 `checkBoundary`（可序列化 + 体积上界） | ✅ 完成（§11.8） |

---

## 附：关键事实速查

| 事实 | 值 |
|---|---|
| 上游版本 / 包名 | `@tintinweb/pi-subagents` v0.19.0 |
| 上游仓库 | `https://github.com/tintinweb/pi-subagents` |
| prux 扩展名 / 优先级 | `subagent` / `PRIORITY_SUBAGENT = 80` |
| 扩展默认状态 / 模式 | `default_enabled = false`，`modes = [Dev, Creator]` |
| 工具名 | `Agent` / `get_subagent_result` / `steer_subagent` |
| 命令 | `/agents`（一/二级菜单）、`/agents types`、`/agents result <id>`、`/agents stop <id>`、`/agents settings`、`/agents edit <type>` |
| 项目级 agent 目录 | `<cwd>/.prux/agents/`（受信任门控） |
| 全局 agent 目录 | `<agent_dir()>/extensions/agent/`（即 `~/.config/prux/extensions/agent/`） |
| 后台并发上限 | 配置 `max_concurrent`（默认 10） |
| 前台并发上限 | 配置 `foreground_max_concurrent`（默认 0 = 不限） |
| 扩展配置文件 | `agent_dir()/extensions/subagent.json`（首次启用落盘默认模板） |
| widget 可见范围 | 配置 `widget` = `all`(默认) / `background` / `off` |
| grace turns | 配置 `grace_turns`（默认 5） |
| `run_in_background` 默认 | 配置 `background_default`（**默认 `true` = 后台**，对齐上游） |
| `persist_session` 默认 | `true` |
| `verbose` 上限 | 200 行 / 8KB |
| 状态集 | `queued` / `running` / `completed` / `steered` / `aborted` / `stopped` / `error` |
| 通知分组 | 配置 `join` = `smart`(30s，默认) / `group`(15s) / `async` |
| 完成卡片 | `CustomMessage` + `render_custom_message`（`custom_type = "subagent-completion"`），随会话落盘并在 `/resume` 后重放（追加在末尾） |
| 交互面 | `/agents → Fleet view`（Inline 覆盖层）、Enter 进入全屏查看器（`i` 聚焦内联输入、`m` 循环 Markdown 渲染） |
| 覆盖层按键 | 扩展优先；TUI 默认：Esc 关闭、↑↓/PgUp/PgDn/Home/End 滚动、字符/退格编辑、Enter 提交 |
| 覆盖层多行编辑器 | `OverlayView.editor`（`OverlayEditor`）：核心持缓冲与光标，Ctrl+S 经 `OverlayEvent::EditorSubmit` 回传整段文本；类型 Edit / 向导系统提示步用 |
| Tier3 引擎 | `rquickjs 0.12.2`（需 C 编译器；无需 libclang），见 §7.1 |

---

## 十一、剩余项与明确不做的功能（全量审计）

> 本节是逐行核对上游 56 个文件与第 2 章特性表后的**最终结论**：已交付见第 8 章，
> 有意偏离见第 3 章。**S7（本轮）**已把此前记录的 7 个真实缺口全部补齐
> （见 §11.1 的“已补齐”列）。

### 11.1 真实缺口 → S7 全部补齐

| # | 项 | 上游落点 | S7 落点 |
|---|---|---|---|
| G1 | 工作流 `agent({ isolation })` | `workflow/worker-source.ts` | `glue.js` 的 `AGENT_OPTIONS` 加入 `isolation`（只接受 `"worktree"`）；`AgentRequest.isolation` → `SpawnRequest.isolation`（frontmatter 优先；`worktree_isolation` 关掉时由 manager 丢弃） |
| G2 | 嵌套 `workflow()` 全局（一层） | `worker-source.ts` + `runtime.ts` | `glue.js` 的 `workflow(nameOrRef, args?)`；`bridge.rs` 的 `run_nested` 递归运行子脚本，共享配额/阶段序号/取消/progress，上限 256、一层；工具层提供 `saved`+`meta` 解析器 |
| G3 | 未 await 的 `agent()` 启动检测 | `runtime.ts` 的 `unawaitedLaunchMessage` | `glue.js` 的 `openLaunches` + `__checkUnawaited()`；`bridge.rs` 的 `wrap_body` 在报告成功前检查，未认领的启动判运行失败 |
| G4 | 设置 `fleetView` / `rememberAgents` / `viewerMarkdown` | `settings.ts` | `config.rs` 三键；`/agents` 菜单按 `fleet_view` 隐藏入口、`persist_session` 缺省取 `remember_agents`、查看器按 `viewer_markdown` 渲染（`m` 键循环） |
| G5 | `defaultMaxTurns` / `graceTurns` 设置 | `settings.ts` | `config.rs` 两键；`SubAgentSpec.grace_turns`（核心）+ `build_spec` 的 `default_max_turns` |
| G6 | 创建向导 "Generate with Claude" | `agent-file-toggle.ts` | `wizard.rs` 汇总步 `g` 把生成提示交给主模型写盘（手动路径不变） |
| G7 | join 合并通知的 Σ 合计 | `index.ts` 的 groupJoin | `notify::batch_envelope` 外层带 `tokens`/`duration`；`manager::BatchItem::Rendered` 带上用量 |

### 11.2 S7 有意保留的偏离

| 项 | 差异 | 理由 |
|---|---|---|
| 嵌套 `workflow()` 不共享并发/Journal/控制面 | 子脚本有自己的并发信号量与 journal（既不重放也不记录）、不进检查器控制面 | 共享并发会要求把信号量/Journal/控制面从 `run_script` 提到跨运行结构；能力可复用的核心价值（组合脚本、共享配额与 UI）已达成，其余留待有需求时再收 |
| 嵌套脚本的代理在 progress 里与父脚本同层 | 不过滤出“子工作流”段 | 复用同一份 `progress` 日志与 `wf-agent-N` 序号，避免 UI 索引撞车；阶段树按共享的 `__allocPhase` 不冲突 |
| `viewer_markdown` 用核心轻量 Markdown 渲染器 | 不复用聊天区 `render/markdown.rs` | 那份需要 `Theme` 且产出 `Line<Span>`，扩展拿不到；`core/extensions/markdown.rs` 产出主题键化的 `DockLine`，与聊天区共享颜色键但支持面较小（无 modifiers/表格/数学） |

### 11.3 明确不迁移（➖）

| 项 | 理由 |
|---|---|
| `child-context.ts`（`AsyncLocalStorage`） | Rust 无对应物；身份改由 `ToolExecCtx.agent_id/depth` 显式传递 |
| `env.ts`（git/平台探测） | 并入 `subagent/worktree.rs`；prux 无独立平台分支需求 |
| `isolation: "remote"` | 上游 Claude Code 的另一种模式，pi 自身也未实现 |
| `perf/*` 基准 | prux 无对应基准基建 |
| 占用 Claude Code 的工具名 `Workflow` | 只做让位检测（`collisions`），保留 `SubagentWorkflow` |

### 11.4 有意偏离（已实现，保留差异）

见 §3.2/3.3/3.4/3.5/3.10/3.12/3.13 与第 8 章各切片的“有意偏离”小节。

### 11.5 S7b 复审补齐的契约缺口

| # | 缺口 | 上游依据 | 收口 |
|---|---|---|---|
| H1 | 嵌套委派未按 `allowed_subagents` 闸门（prux 按深度默认开启；上游 opt-in） | README `Nested subagents`：写了 `allowed_subagents` 才拿到委派工具 | `build_spec` 只在 `ty.allowed_subagents.is_some()` 且非 `isolated` 且未到深度上限时放行；`None`/`false` = 不开启，`all`/`*` = 开启不限型，列表 = 开启且限型 |
| H2 | `serialize_agent_file`（eject/向导）丢字段：`allowed_subagents`/`extensions`/`exclude_extensions`/`skills`/`memory`/`isolation` | `agent-file-toggle.ts::serializeAgentFile` 逐个写出 | 序列化补齐这些字段（`extensions: false`、`skills: false`、`isolation:` 等），往返用例覆盖 |
| H3 | 自定义工具描述占位符与上游不符（缺 `{{agentDir}}`/`{{compactTypeList}}`/`{{isolationGuideline}}`/`{{scheduleGuideline}}`） | README `toolDescriptionMode` 的 `custom` 占位符表 | 补齐上游四个占位符，保留 prux 早期的 `{{projectAgentsDir}}`/`{{globalAgentsDir}}`；`guideline` 在两个开关关掉时展开为空 |
| H4 | 工作流里真失败的子代理被交回**空字符串**而不是 `null` | 工具描述与 README：“Returns null if the subagent fails” | `run_one_agent` 检查记录状态，非 `completed`/`steered` → `AgentReport::failed`（脚本侧 `null`） |

### 11.6 仍保留的偏离（复审后仍不做/降级）

| 项 | 差异 | 理由 |
|---|---|---|
| `gate` + `isolation` | gate 在工具调用的 cwd 跑，不在 agent 的 worktree 副本里 | worktree 生命周期归 `manager`（跑完即清理），gate 在 workflow runner 侧；要让 gate 进副本需把 gate 下沉到 manager 的 worktree 收尾之前，属跨层改动，暂记为偏离 |
| 嵌套 `workflow()` | 不共享并发/Journal/检查器控制面 | 见 §11.2 |
| 记录 GC / handle tombstone | prux 会话内不回收记录（handle 一直可解析） | 上游为限内存而回收 + 落盘 tombstone；prux 不做回收，语义上 handle 反而更持久 |
| FleetView / 查看器入口与键 | prux 走 `/agents → Fleet view`；停止键 `s`（上游 `x` + 确认） | 复用核心既有菜单/覆盖层原语，不新造全局键接管 |

### 11.7 S7c 全量复审补齐

| # | 缺口 | 上游依据 | 收口 |
|---|---|---|---|
| I1 | 工作流沙箱未禁用代码生成（`eval` / `new Function` 可直接用） | README「Scripts run in a … sandbox where … `eval` throws」；`worker-source.ts` 的 `codeGeneration: { strings: false, wasm: false }` | 前奏把 `globalThis.eval` / `globalThis.Function` 改成抛错（与上游一样是卫生措施，`Object.constructor` 的漏洞两边都承认） |
| I2 | 类型管理只有 eject/enable/disable 子命令，没有上游的 Delete / Reset to default，也没有逐代理操作菜单 | `index.ts::showAgentDetail` 的 `Delete` / `Reset to default` / `Edit`；`agent-file-toggle.ts` | `/agents types` 改为交互列表 → 二级操作菜单；新增 `delete` / `reset` 子命令与 `delete_type`/`reset_type`。**Edit 由 S7d 补上（见 §11.8）** |
| I3 | 嵌套子代理的 token 花费未上卷到父代理 | README `Nested subagents`：「their … token spend roll up to it」 | `manager::finish` 在子代理收尾时把其 `usage` 合并进父记录（同锁内，避免竞态） |

### 11.8 S7d 全量复审补齐

| # | 缺口 | 上游依据 | 收口 |
|---|---|---|---|
| J1 | 覆盖层没有多行 editor 原语（单行 `OverlayInput` 只够 steer） | 上游类型详情菜单的 `Edit` 与手动向导的系统提示都用多行编辑器 | 核心新增 `OverlayEditor`（`OverlayView.editor`）+ `OverlayEvent::EditorSubmit` + `OverlayKey::Left/Right/Delete`：核心持编辑缓冲与光标与折行渲染，扩展给初值/标签、按 `generation` 重新装载、Ctrl+S 拿整段文本。复用既有 `editor::Editor` 的移动/删词/翻页 |
| J2 | 类型管理的 `Edit` 没做 | `index.ts::showAgentDetail` 的 `Edit` | `/agents edit <type>` 与二级菜单 `edit`：在 Fullscreen 覆盖层里编辑整个 `.md`（frontmatter + 正文），Ctrl+S 整文件写回 |
| J3 | 手动向导的系统提示正文限单行 | 上游用多行 editor | 向导新增 `System prompt` 步（第 7 步），用多行编辑器；`editor_submit` 存正文，空则回退 Description，生成路径把正文一并交给模型 |
| J4 | 工作流边界只靠 `JSON.stringify`（会静默降级 NaN/Map/稀疏数组/函数）且无体积上界 | `worker-source.ts` 的 `checkBoundary`（args/schema/返回值）；`json-schema.ts` 的 `MAX_SCHEMA_BYTES` | `glue.js` 移植 `checkBoundary`（循环/非有限数/undefined/BigInt/符号/函数/符号键/稀疏数组/非纯对象），并对 schema、嵌套 `workflow()` 的 args 与返回值调用；args 与返回值另加 512KiB 体积上界（上游无此上限，属 prux 附加，理由见下） |

> J4 的体积上界：上游只在 `checkBoundary` 里查可序列化、体积上限只在 schema（64KiB）。prux 的结果要写进 journal/progress 并随会话落盘，超大结果会成倍放大内存与持久化开销，故对 args/返回值加 512KiB（与上游 `MAX_SCRIPT_LENGTH` 同量级）——这是**有意附加**的偏离，超出即整次运行失败并给出字节数。

### 11.9 仍缺（S7d 复审后）

| 项 | 缺什么 | 为什么还没做 |
|---|---|---|
| 记录 GC / `clearCompleted` | 已消费/已完成的记录自动回收 | prux 会话内不回收（见 §11.6），handle 反而更持久；将来长会话内存成为问题再加 |
| `gate` 在 worktree 内运行 | 见 §11.6 | 同上 |
