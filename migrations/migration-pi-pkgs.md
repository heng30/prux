# pi.dev 插件清单：prux 尚未实现、但值得做的 20 个

> **数据来源**：`https://pi.dev/packages` 前 12 页（默认按下载量排序，共 600 个包），
> 逐卡抓取包名 / 描述 / 类型 / 月下载量 / GitHub 地址；描述与行为另用 npm registry 的
> `description`、`readme` 与仓库 README 交叉核对。抓取时间 **2026-10-25**。
>
> **性质**：候选池，不是迁移计划。每项的「价值评分」是本仓视角的判断，不是上游口碑；
> 结论只到「值不值得立一份迁移指南」为止，具体裁剪留给各自的 `migration-*.md`。
>
> **筛选口径**（三条同时成立才收录）：
> 1. `src/extensions/` 与 `src/core/` 下没有等价实现（只部分重叠的会在每项里写明差在哪）；
> 2. 能力对 prux 这种「单机 TUI 编码代理」有实际收益，不是生态碎片（provider 适配、
>    theme、又一份 footer 一律不收）；
> 3. 上游仍在维护（近 4 个月内发过版），且行为有文档可核对。

---

## 〇、先划清「已有」的边界

以下能力本仓已经实现，判断「缺不缺」时不必重复调研：
`mcp` / `web_access` / `browser` / `computer` / `subagent` / `tasks` / `goal` / `plan_mode` /
`lsp` / `memory` / `permissions` / `usage` / `footer` / `notify` / `skills` / `session_search` /
`rewind` / `recap_btw` / `cache_usage` / `cache_watch` / `codemode` / `markit` / `ssh_remote` /
`vet` / `doctor` / `jet` / `ponytail` / `hang_detect` / `loop_detect` / `tool_search` / `proxy`。

三个容易误判的边界：

| 上游看起来「我们有了」 | 实际差别 |
|---|---|
| `pi-lens` vs 本仓 `lsp` | 本仓只用 LSP 做诊断与导航，**没有** linter / formatter / 类型检查器聚合，也没有「写后自动运行」的反馈回路 |
| `@plannotator/*` vs 本仓 `plan_mode` | `plan_mode` 只做只读闸门与计划持久化，**没有**浏览器里的标注 / 逐条批注审批 |
| `rewind` vs `pi-workspace-history` | `rewind` 只移动会话分支指针，**不碰工作树**；上游要的是「文件 + 对话一起回滚」 |

## 一、评分口径

- **价值 1–5**：5 = 能力缺口明显且几乎人人会用到；4 = 缺口明确、收益可量化；
  3 = 有价值但场景窄或与现有能力部分重叠；≤2 不收录。
- **移植成本**：低（单个工具 + 简单 UI）/ 中（新协议适配或状态机）/ 高（需要外部
  二进制、平台隔离能力或跨进程通信）。

---

## 二、总表

| # | 包 | 类型 | 月下载 | 价值 | 一句话 |
|---|---|---|---|---|---|
| 1 | `@juicesharp/rpiv-ask-user-question` | extension | 231K | **5** | 让模型用结构化问卷问你，而不是猜 |
| 2 | `@langfuse/pi-observability-plugin` | extension | 249K | **5** | 会话 / 生成 / 工具调用全量 trace 到 Langfuse |
| 3 | `pi-sandbox` | extension | 6.8K | **5** | OS 级（bubblewrap / Seatbelt）文件与网络沙箱 |
| 4 | `pi-advisor-flow` | extension | 70K | **5** | Executor/Advisor 双模型「第二意见」闸门 |
| 5 | `pi-otel` | extension | 12.9K | 4 | 厂商中立的 OpenTelemetry span 树 |
| 6 | `@spences10/pi-redact` | extension | 4.1K | 4 | 工具输出里的密钥在进上下文前被替换 |
| 7 | `pi-multi-account` | extension | 6.6K | 4 | 配额耗尽自动切下一个账号并续跑 |
| 8 | `pi-messenger-swarm` | extension | 3.9K | 4 | 同机多会话的 mesh 通信与频道 |
| 9 | `@plannotator/pi-extension` | extension | 107K | 4 | 浏览器里标注、批注、审批计划 |
| 10 | `pi-hashline-edit-pro` | extension | 18.9K | 4 | 每行 5 字符锚点编辑，拒绝模糊匹配 |
| 11 | `toolflow` | extension | 3.0K | 4 | 超大日志脱水到磁盘 + 重复读取缓存 |
| 12 | `@spences10/pi-context` | extension | 3.3K | 4 | 超大输出进 SQLite 侧车，只回执摘要 |
| 13 | `pi-durable-subagents` | extension | 6.1K | 4 | 崩溃安全、至多执行一次的子代理 |
| 14 | `@upstash/context7-pi` | extension | 4.0K | 4 | 库 / 框架最新文档的按需检索 |
| 15 | `@henryqw/pi-pr` | package | 7.1K | 4 | `/pr` 打通发现、修复 CI、合并的闭环 |
| 16 | `layermap` | package | 3.7K | 4 | 改动影响面：谁调用它、波及哪些路由 |
| 17 | `pi-workspace-history` | extension | 3.3K | 4 | `/undo` 把文件和对话一起退回 |
| 18 | `pi-lens` | extension | 88.8K | 3 | 写后实时 lint / format / 影响级联诊断 |
| 19 | `pi-simplify` | extension | 37.9K | 3 | 只审改动行的简化审查 |
| 20 | `pi-autoresearch` | extension | 5.2K | 3 | 自动「试—测—留—弃」优化循环 |

---

## 三、逐项

### 1. `@juicesharp/rpiv-ask-user-question` — 结构化提问

- **GitHub**：https://github.com/juicesharp/rpiv-mono （monorepo，包在 `packages/` 下）
- **npm**：https://www.npmjs.com/package/@juicesharp/rpiv-ask-user-question
- **类型 / 规模**：extension｜231.4K/月｜v2.15.0（2026-10-24）
- **描述**：给模型一个 `ask_user_question` 工具，弹出最多四题的终端对话框，选项是写好的、
  不是自由发挥；答案以结构化数据回传，支持单选、多选、逐题备注与全局备注。
- **主要用途**：把「歧义」挡在动手之前。需求里埋着真实决策（加缓存的策略、改配置的取舍）时，
  模型先问而不是替你选，省下的是返工。
- **prux 现状**：没有 ask-user 类工具。现有交互只有扩展 UI 请求、`/` 命令面板与权限确认，
  模型无法主动发起带类型的问卷。
- **价值 5｜成本 中**：缺口在「人机协议」这一层，任何有歧义的任务都会碰到；实现要动
  `ExtensionUiRequest` 的形态（多题 + 多选 + 备注）与 TUI 渲染，不是加个工具壳就完。
- **移植要点**：问卷答案要能作为 tool result 回到同一轮；多选与备注要可序列化进会话历史
  （压缩后仍要可读）。

### 2. `@langfuse/pi-observability-plugin` — 会话可观测性

- **GitHub**：https://github.com/langfuse/pi-observability-plugin
- **npm**：https://www.npmjs.com/package/@langfuse/pi-observability-plugin
- **类型 / 规模**：extension｜249.3K/月｜v0.1.2（2026-08-31，上游自标 experimental）
- **描述**：监听会话生命周期事件，把每个用户提示作为一条 trace 发到 Langfuse：agent 轮次、
  模型生成（输入输出、成本、首 token 时间、含缓存命中与 reasoning 的 token 拆分）、
  工具调用（失败标 `ERROR`）、图片作为 media 上传、子代理嵌套在发起它的轮次下。
- **主要用途**：团队排障与成本核算。出问题时要能回答「这次会话烧在哪、哪一步变慢、
  哪个工具在反复失败」，而不是翻屏。
- **prux 现状**：无任何 trace 导出。`usage` 只汇总本会话用量，`cache_watch` 只看缓存命中，
  都没有跨会话、跨轮次的结构化落点。
- **价值 5｜成本 中**：埋点面（轮次 / 生成 / 工具 / 子代理）在 `core::agent_session` 与
  provider 流式回调里都能拿到；难的是不要把网络发送放进主循环的关键路径。
- **移植要点**：默认关闭；发送走后台队列；失败必须静默降级，绝不能拖慢或中断会话。

### 3. `pi-sandbox` — OS 级沙箱

- **GitHub**：https://github.com/carderne/pi-sandbox
- **npm**：https://www.npmjs.com/package/pi-sandbox
- **类型 / 规模**：extension｜6.8K/月｜v0.7.2（2026-10-20）
- **描述**：`read` / `write` / `edit` 用 allow/deny 列表直接管控；`bash` 交给
  `sandbox-runtime`（Anthropic `sandbox-runtime` 的 fork）限制网络与文件系统访问。
  被拦的调用弹临时 / 永久授权，bash 的文件系统失败原样返回、不自动重试。
- **主要用途**：把「模型能碰什么」从提示词约定变成内核级约束。当前 agent 的权限模型再细，
  也是一层用户态策略；`bash -c 'curl … | sh'` 这类绕过只能靠系统隔离兜。
- **prux 现状**：`permissions` 扩展是策略式命令拦截（匹配命令、路径、危险模式），
  `codemode` 的 sandbox 只隔离 JS 执行；`bash` / `powershell` 子进程没有任何 OS 级隔离。
- **价值 5｜成本 高**：Linux 走 bubblewrap、macOS 走 Seatbelt，Windows 没有对等物
  （要么不做、要么容器）；三平台行为差异需要单独写清，否则就是「安全幻觉」。
- **移植要点**：先做 Linux + macOS 两平台，配置沿用现有 `permissions` 的信任模型；
  沙箱不可用时必须显式报错，不能静默回退成无沙箱。

### 4. `pi-advisor-flow` — Executor / Advisor 双模型

- **GitHub**：https://github.com/philipbrembeck/pi-advisor
- **npm**：https://www.npmjs.com/package/pi-advisor-flow
- **类型 / 规模**：extension｜70.2K/月｜v0.14.2（2026-10-21）
- **描述**：执行仍由快模型负责，另起一个更强的模型做「第二意见」：`ask_advisor` 工具与
  `/advisor-manual` 手动咨询；可配闸门（计划前、连续失败后、宣布完成前）；重复工具调用
  触发循环检测并给出 proceed / revise / blocked；Executor 与 Advisor 的模型和 reasoning
  分开配置；同模型咨询默认跳过；`followUpTo` 复用去敏后的上下文前缀；带用量与成本统计。
- **主要用途**：把「贵模型」用在决策点而不是每一次生成。长任务里最贵的错误是方向错，
  这里用一次便宜的结构化复审换掉整段返工。
- **prux 现状**：有 `subagent`（可指定模型）与 `loop_detect`，但没有「主模型向另一个模型
  征询、按闸门自动触发、并把结论作为建议注入」的机制；subagent 是分派任务，不是征询意见。
- **价值 5｜成本 中**：需要新的 hook 位（计划前 / 失败后 / 结算前）与「建议不进主上下文太长」
  的裁剪策略；`plan_mode`、`vet` 的边界钩子可以复用。
- **移植要点**：默认关闭；咨询必须可缓存、可跳过（同模型、超预算、用户拒绝）；
  隐私开关（历史、仓库上下文、`AGENTS.md`、工具结果）要与现有信任模型对齐。

### 5. `pi-otel` — OpenTelemetry 导出

- **GitHub**：https://github.com/nikiforovall/pi-otel
- **npm**：https://www.npmjs.com/package/pi-otel
- **类型 / 规模**：extension｜12.9K/月｜v0.3.0（2026-09-13）
- **描述**：按提示词建 span 树（interaction → `llm_request` → `tool.<name>`），覆盖
  `gen_ai.*` 语义约定（token、成本、模型、finish reason、工具调用），OTLP 导出，
  gRPC / http-protobuf / http-json 三种协议，可配 `captureContent: metadata_only` 与采样率；
  `/otel start` 还能拉起本地 Aspire dashboard。
- **主要用途**：不绑定单一厂商的观测落地。Langfuse 是 SaaS/SDK 绑定，OTel 能进
  Grafana / Aspire / 任何 collector，是「不想把数据交给第三方」时的默认解。
- **prux 现状**：无。
- **价值 4｜成本 中高**：协议本身不复杂（OTLP + GenAI 语义约定），但要把 span 生命周期
  和现有的流式回调、子代理、工具执行对齐；内容默认只留元数据，避免把代码送出去。
- **移植要点**：与第 2 项共用同一套埋点；两者择一先做，另一套只换 exporter。

### 6. `@spences10/pi-redact` — 密钥脱敏

- **GitHub**：https://github.com/spences10/my-pi （monorepo，包在 `packages/pi-redact`）
- **npm**：https://www.npmjs.com/package/@spences10/pi-redact
- **类型 / 规模**：extension｜4.1K/月｜v0.0.15（2026-08-23）
- **描述**：在 `tool_result` 与 `!` shell 输出进入模型上下文之前扫描可能的 token / 密钥 /
  凭据，替换成安全的占位符，同时保留足以排障的上下文。
- **主要用途**：`cat .env`、`env`、CI 日志、`npm config list` 都可能把密钥带进上下文，
  进而进入第三方 API 与日志。这是最后一道闸。
- **prux 现状**：`shake.rs`、`proxy` 里有零散的 secret 处理，但没有统一的「工具输出 →
  脱敏 → 入上下文」拦截点；`toolflow` 类保护也不存在。
- **价值 4｜成本 低**：一个输出钩子 + 一组高精度正则/熵检测；难点是误报（把正常代码里的
  hash 打成占位符），需要 allowlist 与「已脱敏」标记。
- **移植要点**：只处理工具输出，不改用户输入；脱敏后的占位符要在 UI 里可见地标注，
  否则用户会以为原文如此。

### 7. `pi-multi-account` — 多账号轮换与故障转移

- **GitHub**：https://github.com/Sarrius/pi-multi-account
- **npm**：https://www.npmjs.com/package/pi-multi-account
- **类型 / 规模**：extension｜6.6K/月｜v1.26.0（2026-10-19）
- **描述**：自动发现已认证的多个账号（Anthropic、OpenAI/ChatGPT、Codex、Kimi、Cursor、
  Qwen、Ollama），组成轮换队列；配额或限流命中时切下一个账号，可选地继续被打断的任务；
  `/login` 新账号后自动扩展轮换；支持按 slot 命名。
- **主要用途**：订阅制账号有配额上限，长任务不该因为一个账号耗尽而整段失败。
- **prux 现状**：`model_resolver` 只解析「用哪个模型」，鉴权是每 provider 一份凭据；
  没有多账号池、没有配额感知、没有失败切换后继续。
- **价值 4｜成本 中高**：要改鉴权层（凭据存储、选择策略、切换时机），并保证切换后
  会话与压缩状态不串；与 `proxy`（入站网关）方向相反，别混在一起。
- **移植要点**：切换必须发生在一次请求边界，不能打断流；切完要有可读的 UI 提示，
  否则用户会以为模型换了个「性格」。

### 8. `pi-messenger-swarm` — 同机多会话 mesh

- **GitHub**：https://github.com/monotykamary/pi-messenger-swarm
- **npm**：https://www.npmjs.com/package/pi-messenger-swarm
- **类型 / 规模**：extension｜3.9K/月｜v0.26.9（2026-10-16）
- **描述**：基于文件的、无需 daemon 的多 agent 协同：加入 mesh、共享频道（memory / session）、
  派生 subagent，会话之间可以传上下文与结论。
- **主要用途**：一个人同时开「调研 / 实施 / 复核」三个会话时，结论能直接递过去，
  而不是复制粘贴。与 subagent 的区别是「同侪会话」而不是「下属进程」。
- **prux 现状**：`subagent` 有父 → 子分派与 `steer`，但多个顶层会话之间没有通信通道；
  `session_search` 只能事后检索，不能实时投递。
- **价值 4｜成本 中**：文件锁 + 收件箱 + 唤醒（空闲时投递、忙碌时排队）是核心；
  要防重复消费与死信堆积。
- **移植要点**：优先做「用户手动发」与「agent 主动发」两种入口；消息要有来源会话 ID，
  否则多会话互相污染时无法溯源。

### 9. `@plannotator/pi-extension` — 计划标注与审批

- **GitHub**：https://github.com/backnotprop/plannotator
- **npm**：https://www.npmjs.com/package/@plannotator/pi-extension
- **类型 / 规模**：extension｜107.1K/月｜v0.28.30（2026-10-24）
- **描述**：文件化的计划模式 + 浏览器 UI：审阅、逐条标注、批注 agent 消息、审批计划，
  也能审代码/PR；计划落盘，不随会话消失。
- **主要用途**：复杂改动先在「可指摘的界面」上过一遍。终端里逐条说自己不满意哪一段，
  成本远高于在计划上点点划划。
- **prux 现状**：`plan_mode` 有只读闸门与计划持久化，但审阅是纯文本，没有标注、
  没有逐条回传、没有浏览器视图。
- **价值 4｜成本 中高**：要起本地 HTTP 服务 + 前端资源 + 与 TUI 的状态同步；
  与 `browser` / `computer` 的能力边界要划清（这是产品自带 UI，不是让模型去点网页）。
- **移植要点**：审批结果必须落回会话（而不是只存浏览器里）；计划文件位置沿用
  `plan_mode` 的既有约定，别造第二套。

### 10. `pi-hashline-edit-pro` — 哈希锚定编辑

- **GitHub**：https://github.com/YuGiMob/pi-hashline-edit-pro
- **npm**：https://www.npmjs.com/package/pi-hashline-edit-pro
- **类型 / 规模**：extension｜18.9K/月｜v7.0.0（2026-10-23）
- **描述**：`read` 返回的每一行带一个唯一的 5 字符锚点，`replace` / `insert` / `grep` 都按
  锚点寻址；锚点在编辑后保持稳定，**过期或歧义的锚点直接拒绝，绝不模糊匹配**；撤销跨重启
  可用。
- **主要用途**：消灭「改错行」。行号在多次编辑后会漂移，字符串匹配会撞上重复片段，
  锚点是这两类事故的结构性解法。
- **prux 现状**：只有基于精确文本匹配的 `edit`（要求 oldText 唯一）与 `write`；
  没有行锚点、没有 stale 检测、没有编辑撤销栈。
- **价值 4｜成本 中**：要改 `read` 的输出格式（每行前缀）+ 编辑工具的寻址方式，
  并且**必须**让旧格式输出在压缩后仍可读；锚点分配要进会话状态。
- **移植要点**：锚点算法要抗 tokenizer（上游特意选了小写 5 字符）；拒绝时的报错要给出
  「重新读一次」的可操作提示，而不是只说 anchor stale。

### 11. `toolflow` — 工具输出脱水与能力分层

- **GitHub**：https://github.com/3ZEROS12/toolflow
- **npm**：https://www.npmjs.com/package/toolflow
- **类型 / 规模**：extension｜3.0K/月｜v3.3.5（2026-10-11）
- **描述**：把超大 bash / grep 日志脱水到磁盘（宣称 -95% token 开销）、缓存重复文件读取、
  按阶段挂载能力（未用到的工具不进 system prompt）、保护 `.env` / `.git` 误删，
  另带 prompt 工作台。
- **主要用途**：两个真实痛点——一是 `cargo build` / `grep -r` 的输出把上下文吃光；
  二是 40+ 工具的 schema 常驻系统提示词，噪声比能力更贵。
- **prux 现状**：`utils::truncate` 只做截断（丢弃而非保留可回查），`tool_search` 做工具
  延迟暴露，但没有「输出落盘 + 回执 + 按需检索」的闭环；重复读取没有缓存。
- **价值 4｜成本 中**：落盘 + 回执链接 + 检索工具；关键在回执要能定位（文件路径 + 行范围），
  并且清理策略要防磁盘无限增长。
- **移植要点**：与 `tool_search`（延迟暴露）职责分开；与第 12 项择一，别做两套侧车。

### 12. `@spences10/pi-context` — SQLite 输出侧车

- **GitHub**：https://github.com/spences10/my-pi （monorepo，包在 `packages/pi-context`）
- **npm**：https://www.npmjs.com/package/@spences10/pi-context
- **类型 / 规模**：extension｜3.3K/月｜v0.1.16（2026-08-23）
- **描述**：把超大的命令 / 文件 / MCP / LSP 结果存进本地可搜索的 SQLite 侧车，只把紧凑回执
  交给模型；模型需要时可搜索或按需取回；宣称在文档抽取基准上 token 降 68%、成本降 54%。
- **主要用途**：与第 11 项同类，但走「可查询索引」而不是「文件 + 路径」。
- **prux 现状**：无；`session_search` 是搜历史会话，不是搜工具输出。
- **价值 4｜成本 中**：需要 sqlite 依赖与 FTS5（或等价索引），以及「什么算超大」的阈值
  与去重策略。中文分词要考虑，否则中文日志检索退化成 like。
- **移植要点**：索引库放 `agent_dir()` 下并纳入清理；回执必须带可点击/可复制的定位信息。

### 13. `pi-durable-subagents` — 崩溃安全子代理

- **GitHub**：https://github.com/purboo/pi-durable-subagents
- **npm**：https://www.npmjs.com/package/pi-durable-subagents
- **类型 / 规模**：extension｜6.1K/月｜v1.4.0（2026-10-24）
- **描述**：子代理的「至多一次」执行：请求只决策一次、步骤最多完成一次；流断、请求超时、
  模型空回复、用户退出、机器重启后从断点续跑；同一会话内恢复；带实时视图；
  上游自带 9 个故障注入场景的自测（重复运行 0、丢结果 0、从头重跑 0）。
- **主要用途**：长时子任务最怕「跑到一半进程没了」。这里把「重试 / 恢复」做成有日志的
  确定性机制，而不是靠人再喊一遍。
- **prux 现状**：`subagent` 有 worktree、`resume` 与后台运行，但恢复粒度是「已有 session
  可以续」，没有步骤级日志、没有去重保证，也没有故障注入测试。
- **价值 4｜成本 高**：要设计 append-only 的步骤日志（journal）+ 幂等边界；
  「工具对外部世界的副作用」是无法保证的部分，必须在文档里写清。
- **移植要点**：复用现有 `subagent` 的会话存储，别另起目录；恢复入口沿用 `resume`。

### 14. `@upstash/context7-pi` — 库文档按需检索

- **GitHub**：https://github.com/upstash/context7
- **npm**：https://www.npmjs.com/package/@upstash/context7-pi
- **类型 / 规模**：extension｜4.0K/月｜v0.1.3（2026-10-13）
- **描述**：官方 Context7 扩展，提供 `resolve-library-id`（包名 → 库 ID）与 `query-docs`
  （取该库最新文档与代码示例）两个工具，附一个教模型「何时该查文档」的 skill 与
  `/c7-docs` 命令；无 key 时按 IP 限流可用，有 key 提配额。
- **主要用途**：模型对库 API 的记忆常常是旧版本；与其猜签名，不如按需拉最新文档。
  比通用 web 搜索更准（来源是库自己的文档）。
- **prux 现状**：`web_access` 有搜索与抓取，`markit` 能读文档，但没有「库 → 结构化文档」
  的解析层；模型没有专门的 `query-docs` 工具。
- **价值 4｜成本 低**：本质是两个 HTTP 工具 + 一个 skill + 系统提示引导；难点是
  「先 resolve 再 query」的两步协议要稳定。
- **移植要点**：作为 `web_access` 的补充而不是替代；离线/失败要能降级成普通搜索。

### 15. `@henryqw/pi-pr` — PR 闭环

- **GitHub**：https://github.com/HenryQW/pi-harness
- **npm**：https://www.npmjs.com/package/@henryqw/pi-pr
- **类型 / 规模**：package｜7.1K/月｜v10.5.7（2026-10-24）
- **描述**：底栏常驻当前分支的 PR 状态；`/pr` 读最新证据（PR、CI、review）并选下一步：
  发现/关联 PR、创建、更新、处理反馈、修 CI、以及需确认的合并；依赖 `gh` 已认证。
- **主要用途**：把「改完 → 开 PR → 等 CI → 修红 → 合并」收成一条命令流，
  省掉在 TUI 与浏览器之间来回。
- **prux 现状**：无 PR 相关能力；`bash` 里手动敲 `gh` 当然可以，但没有状态、没有
  失败重试、没有「修 CI 直到绿」的收敛条件。
- **价值 4｜成本 中**：`gh` CLI 是依赖面（要处理未安装 / 未认证 / Enterprise）；
  修 CI 必须有上限与「同一条失败不重复」的判定，否则烧 token。
- **移植要点**：合并必须二次确认；CI 修复复用 `vet` 的收敛条件设计。

### 16. `layermap` — 改动影响面

- **GitHub**：https://github.com/coffeecoproject/layermap
- **npm**：https://www.npmjs.com/package/layermap
- **类型 / 规模**：package｜3.7K/月｜v0.1.31（2026-10-23）
- **描述**：给出「谁调用这个函数、改它会影响什么（一直到 HTTP 路由）」的分层地图；
  由各语言编译器/类型检查器解析（不是 tree-sitter 猜名字），覆盖 TypeScript / JS /
  Go / Python / Java；支持接口、基类、模板、装饰器。
- **主要用途**：大仓里改一个函数前先知道影响面。上游基准：15 次请求内找到 97.9% 受影响
  端点，不用时 58.8%。
- **prux 现状**：只有 `lsp` 的 references / definition 与 `find` / `grep`，
  没有跨文件的调用链聚合，也没有「函数 → 路由」的语义映射。
- **价值 4｜成本 高**：多语言编译期解析（要么调 `tsserver` / `gopls` / `pyright` 等外部
  进程，要么自己写轻量索引），维护成本高。
- **移植要点**：先支持一种语言（优先 Rust / TypeScript）验证价值，再谈多语言；
  与 `lsp` 扩展共享语言服务器进程，别各起一套。

### 17. `pi-workspace-history` — 文件 + 对话一起撤销

- **GitHub**：https://github.com/wcldyx/pi-workspace-history
- **npm**：https://www.npmjs.com/package/pi-workspace-history
- **类型 / 规模**：extension｜3.3K/月｜v0.5.2（2026-10-24）
- **描述**：`/undo` 把「从提示词发出到 agent 结束」当一个单位，文件（改、建、删、重命名，
  含 shell 做的改动）与对话一起回退，原提示词回到输入框；`/redo`、`/checkpoint`、
  `/diff`、`/tree`（跳到任意位置并选是否恢复文件）；历史存在项目外的私有 git 仓库，
  不碰用户仓库；用户自己的手动修改不会被覆盖。
- **主要用途**：agent 改崩了工作树时的「一键回到刚才」。这是比「会话回退」强一档的保险。
- **prux 现状**：`rewind` 只移动会话分支指针，**工作树原样保留**；`subagent` 的 worktree
  是并行隔离，不是撤销。
- **价值 4｜成本 中高**：要在每个提示词边界做工作树快照（不能污染用户仓库：独立目录 /
  临时 index），并处理「快照后用户又手改过」的冲突（上游选择拒绝恢复）。
- **移植要点**：快照必须排除 `.gitignore` 与超大文件；与 `rewind` 的语义差异要在
  UI 文案里说清（回退对话 vs 回退文件+对话）。

### 18. `pi-lens` — 写后实时反馈闭环

- **GitHub**：https://github.com/apmantza/pi-lens
- **npm**：https://www.npmjs.com/package/pi-lens
- **类型 / 规模**：extension｜88.8K/月｜v4.4.4（2026-10-19）
- **描述**：跨语言 LSP 诊断与导航、**影响级联诊断**（改完顺带检查受影响文件）、
  每次写/编辑后跑语言专属 linter / 类型检查 / 扫描器、安全的格式化与 autofix、
  ast-grep 与 tree-sitter 结构规则（正确性/安全异味）、`symbol_search` 常热索引、
  诊断分诊（误报 / 源码抑制 / 延后 / 待修）、`/lens-map` 依赖图。
- **主要用途**：让「写错」在下一轮之前暴露，而不是等用户跑测试。这是 `vet` 的
  实时化 + 多语言化。
- **prux 现状**：`lsp` 只做按需诊断/导航；`vet` 只在结算前跑用户配置的命令。
  没有「编辑后自动 lint/format」也没有影响级联。
- **价值 3｜成本 高**：价值高但成本也最高的一个——多语言工具探测、进程池、诊断去重、
  自动修复的安全边界，全都要自己扛；部分能力 prux 已有，属于「增强」而非「补齐」。
- **移植要点**：先做「编辑后跑已配置的 linter」这一层；ast-grep / tree-sitter 规则往后放。

### 19. `pi-simplify` — 改动行简化审查

- **GitHub**：https://github.com/MattDevy/pi-extensions
- **npm**：https://www.npmjs.com/package/pi-simplify
- **类型 / 规模**：extension｜37.9K/月｜v0.2.3（2026-07-17）
- **描述**：`/simplify` 用 `git diff` 找出改动文件与行区间，只审这些行：保持行为不变、
  遵循 `CLAUDE.md` / `AGENTS.md` 约定、降低复杂度、去冗余、改命名；可 `--staged`、
  指定文件、`--ref=main`；新文件全文审；逐文件改并跑测试验证。
- **主要用途**：agent 写完的代码常有「过度设计 / 重复 / 命名糊」，在提交前收一遍。
- **prux 现状**：无；`vet` 只跑检查命令，`ponytail`（懒资深开发）是提示词风格，
  不是「审改动行」的流程。
- **价值 3｜成本 低**：一个命令 + diff 范围提取 + 一段审查提示词，落点清晰；
  价值受限于模型本身，容易变成「再生成一遍」。
- **移植要点**：必须硬性限定在改动行（否则会顺手重构全仓）；每步跑测试的门槛要可配。

### 20. `pi-autoresearch` — 自动优化循环

- **GitHub**：https://github.com/davebcn87/pi-autoresearch
- **npm**：https://www.npmjs.com/package/pi-autoresearch
- **类型 / 规模**：extension｜5.2K/月｜v1.9.0（2026-10-07）
- **描述**：给定一个可测量的目标（测试耗时、包体积、构建时间、Lighthouse 分数），
  循环「提假设 → 改 → 测 → 留或弃」；带工具、实时 widget 与 `/autoresearch` 面板；
  灵感来自 `karpathy/autoresearch`。
- **主要用途**：把「优化」从一次性尝试变成可回滚的实验循环；适合有稳定基准的仓库。
- **prux 现状**：`goal` 是「达成条件即停」，`vet` 是「失败就续跑」，都不是
  「度量 → 保留/回滚」的搜索循环。
- **价值 3｜成本 中**：需要「基线测量 → 改动 → 复测 → 决策 → 回滚」的状态机与
  噪声抑制（否则会把抖动当改进）。场景窄（要求仓库自带可重复基准）。
- **移植要点**：回滚依赖工作树快照（可与第 17 项共用）；必须先固定测量噪声阈值。

---

## 四、建议落地顺序

按「缺口 × 成本」切三批，每批独立可用：

1. **低成本、补协议**：`@spences10/pi-redact`（6）、`@upstash/context7-pi`（14）、
   `pi-simplify`（19）。都是钩子/工具级改动，不动架构。
2. **中成本、补闭环**：`@juicesharp/rpiv-ask-user-question`（1）、`pi-advisor-flow`（4）、
   `pi-workspace-history`（17）、`@henryqw/pi-pr`（15）。各自要新的交互或状态机，
   但复用现有 hook 与命令体系。
3. **高成本、补底座**：`pi-sandbox`（3）、可观测性（2 与 5 择一）、`pi-multi-account`（7）、
   `pi-durable-subagents`（13）。涉及平台隔离、跨进程或鉴权层，需要单独立项。

`toolflow`（11）与 `@spences10/pi-context`（12）二选一，不要同时做两套输出侧车。

## 五、备选（价值 3，先记下不展开）

| 包 | GitHub | 月下载 | 说明 |
|---|---|---|---|
| `@ff-labs/pi-fff` | https://github.com/dmtrKovalenko/fff | 31.6K | FFF 驱动的模糊文件/内容搜索，补 `find`/`grep` 的模糊匹配 |
| `pi-autosuggestions` | https://github.com/0xPlayerOne/pi-autosuggestions | 2.8K | zsh-autosuggestions 风格的输入历史幽灵补全 |
| `pi-markdown-preview` | https://github.com/omaclaren/pi-markdown-preview | 15.2K | Markdown + LaTeX 渲染预览（终端/浏览器/PDF） |
| `pi-voicekit` | https://github.com/CyFeng16/pi-voicekit | 3.7K | 本地按住说话中英 STT + 离线 TTS |
| `pi-vision-handoff` | https://github.com/monotykamary/pi-vision-handoff | 3.4K | 给纯文本模型补视觉：先用视觉模型描述图片再转交 |
| `@narumitw/pi-sync` | https://github.com/narumiruna/pi-extensions | 3.3K | 配置经 Git / WebDAV / R2 / S3 多机同步 |
| `@pi-unipi/kanboard` | https://github.com/Neuron-Mr-White/unipi | 7.5K | 工作流看板（HTTP + htmx UI + TUI 覆盖层） |
| `@postman/postman-plugin` | https://github.com/postmanlabs/postman-plugin | 164.7K | Postman API 工程技能（集合、测试、Mock） |
| `confluence-cli` | https://github.com/pchuri/confluence-cli | 42.1K | Confluence 页面创建与编辑 |
| `openwiki` | https://github.com/langchain-ai/openwiki | 171.9K | 自动生成并维护代码库 wiki |

## 六、看过但明确不做的（省下一次重复调研）

| 包 | 不做理由 |
|---|---|
| `billion-context`（1.8M/月） | 主体是跨 agent 的压缩侧车；本仓 `core::compaction` 已有分层压缩，收益主要是「更长会话」，与现有机制重叠且要另立协议 |
| `context-mode`（91.4K/月） | 沙箱代码执行 + FTS5 知识库；`codemode` 已覆盖 JS 沙箱执行，再引入一套执行面只会分裂 |
| `pi-lens` 之外的 LSP 类（`@narumitw/pi-lsp`、`pi-lsp-adapter`、`@mrclrchtr/supi-code-intelligence`） | 与现有 `lsp` 扩展同生态位，属重复建设 |
| `pi-powerline-footer`、各类 statusline / theme / `supi-*` UI | 底栏与主题已有三布局与内嵌主题机制，纯样式不列入 |
| `pi-mcp-adapter`、`pi-mcp-extension`、`@spences10/pi-mcp` | `mcp` 扩展已实现客户端与配置管理 |
| `pi-web-access`、`pi-web-search`、各类 provider 适配（LiteLLM / NIM / Vertex / DashScope…） | `web_access` 已覆盖搜索/抓取；provider 由 `model_resolver::SUPPORTED_PROVIDERS` 统一管理，不按第三方包逐个对齐 |
| `@tintinweb/pi-tasks`、`@pi-unipi/*` 的 task / goal / btw / memory 变体 | 与 `tasks` / `goal` / `recap_btw` / `memory` 同生态位 |
| `@gotgenes/pi-permission-system`、`cc-safety-net`、`@aliou/pi-guardrails` | `permissions` 已做策略式拦截；这些是同一层的不同规则集，不在「能力缺口」里 |
| `@juicesharp/rpiv-i18n`、`superpowers-zh`、各类 skill 合集 | 内容型包（技能/提示词/翻译），不是扩展能力；本仓 `skills` 机制已能加载外部技能 |
