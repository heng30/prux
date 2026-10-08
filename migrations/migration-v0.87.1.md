# Prux 迁移指南：pi 0.85.1 → 0.87.1

> 本文档仅列出当前 prux 项目**已实现的功能**在 pi 0.85.1→0.87.1 期间的相关变更，
> 以及该区间内 pi **新增的功能**（作为候选移植项）。
>
> 排除规则：prux 没有、且不是 0.85.1→0.87.1 新增的功能，不列出。因此以下 pi 侧内容
> 已刻意省略：prux 未实现的 provider（Amazon Bedrock、Mistral 等）；纯 JS/Node 运行时
> 优化（如 Node 持久编译缓存）；`--mode` CLI 参数、RPC
> `steer`/`follow_up`、llama.cpp `enable_thinking`、`user_bash` 等 prux 不存在的特性；
> 仅属于 `packages/{client,protocol,server,telemetry,durable}` 的改动；以及 pi 扩展体系
> （`pi.on()`/TS 类型导出）中 prux 架构上不存在的能力。
>
> **来源**：`pi/packages/{coding-agent,ai,tui,agent}/CHANGELOG.md` 的
> `[0.86.0]`、`[0.86.1]`、`[0.87.0]`、`[0.87.1]` 段落。
>
> **prux 基线**：`sync.md` = 0.85.1；provider 目录/会话格式的现状以本仓 `assets/models/`、
> `src/core/session_v4.rs` 为准。
>
> **状态标记**：✅ 已确认现状（含需修/已对齐）　⚠️ 需对照 pi 源码逐项核对　❌ 结构性不适用
>
> **更新记录**：第五节「高优先级」8 项已全部实现（见各项 `prux 状态`）。模型目录先做
> **定点补充**，后由用户完成**全量 `sync-models.py` 同步**（`[+] sync to pi-v0.87.1-part2` + meta provider）；
> 随之而来的 `deepseek-v4-flash → deepseek-flash` 等改名已同步更新测试/source 引用，当前 `cargo test` 全绿。
>
> **中优先级审计（第二轮）**：已对四簇（会话/压缩/溢出、provider/流式、TUI/UX、扩展）
> **逐项对照 pi 0.87.1 源码完成审计**，结论见第五节「中优先级审计结果」。
> 无争议项已自动实现（共 13 处）；三项经用户拍板后实现（overflow 三态、Cerebras
> bodyless、CJK 补全、剪贴板门控）；最后三项（Fireworks/Vercel unsigned thinking、
> Google thinking level、LaTeX cases/嵌套上下标）也已实现；新特性类标记「本轮不做」。
>
> **第三轮（移除项）**：第三节「从 OpenAI Codex 目录移除 GPT-5.4」已落地——核实
> prux 的 Codex 目录**并非**来自 `models.openai.json`（而是远程 pi.dev 网关目录，
> 且此前无内嵌基线），本轮补齐内嵌 `assets/models/models.openai-codex.json`
> （由 pi 0.87.1 同步，8 个模型，天然不含 5.4 系列）+ `openai-codex` 默认模型
> `gpt-5.5`，并补回归测试；`openai`（API key）与 `github-copilot` 目录仍保留
> `gpt-5.4`，与 pi `defaultModelPerProvider` 一致。
>
> **第四轮（破坏性变更）**：四.1 `shouldStopAfterTurn` → `finishTurn` 已实现
> （`FinishTurnAction::{End, Continue}`，turn_end 之前调用、决策之后生效，error/aborted
> 硬退出）；四.4 边界事件已实现（`ExtensionHook::Boundary` + `on_boundary`，
> `turn_end` 边界字段与 `agent_before_settle` 派发）；四.3（SessionManager 权威来源）、
> 四.5（agent_settled 重入防护）经审计为 N-A/结构上已保证；四.2 未移植 canonical
> context 前无需改动。
>
> **第五轮（图片输入限制）**：二.15 `inputLimits.images.resize` 已实现
> （`utils::image::ImageResizeLimits` + `prepare_image`；目录 → `ModelEntry`/`ModelConfig`；
> 作用于 read 工具 / CLI·TUI 图片附件 / 工具结果图片）。
>
> **第六轮（边界落地 + 会话加载 + 完整 transcript）**：
> - 四.4 的边界钩子接上**两个内置消费者**：loop-detect 命中后在 `turn_end` 边界返回
>   `end`（本 run 在当前回合结束后确定性终止，不再只依赖 UI 事件循环处理 `AbortRun`），
>   goal 改用 `agent_before_settle` 边界自主续跑（追加续跑触发消息 + `continue`）+
>   `turn_end` 边界起预算收尾轮；边界事件新增 `agentScope` / `hasQueuedMessages` /
>   `continuationIndex` 字段，`BoundaryOutcome` 新增 `append`（pi `BoundaryResult.entries` 子集），
>   结算续跑由"每次 prompt 至多一次"改为 pi 同款循环（扩展自限 + Esc 退出）；
> - 二.14 `context_with_system` 已实现（完整 transcript 变换，结果原样发送）；
> - 二.11 `--resume` 渐进加载（CLI 选择器 + TUI `/resume`）与 `--continue` header 提前停止已实现；
> - 二.16 spinner 确认不实现。
>
> **第七轮（未实现项复核 + 点击切换摘要）**：二.12 Meta（Muse 订阅）provider 经复核**已实现 API key 路径**
> （`assets/models/models.meta.json` + `login_registry` + `META_API_KEY` + `default_model_for`），
> 仅缺 pi 的 Muse OAuth 订阅自动刷新；二.2 `/bug` 改为**待实现**（本地 zip 导出可脱离 Radius 独立做），
> 不再列入「不做」；二.6 折叠块点击切换已实现（Alt+左键，见下）。剩余未实现项按优先级重排，
> 见第五节「剩余未实现项（第十轮定案）」。
>
> **第八轮（文档纠错 + P1 落地）**：
> - 文档纠错：一.1 工具耗时格式由 `m:ss`/`h:mm:ss` 改为 pi 真实实现
>   （`Xm Ys` / `Xh Ym Zs`，`renderers/bash.ts::formatDuration`）；第五节中优先级表
>   的「`--resume`/`--continue` 渐进与加速 | 不做」与二.11 的「✅ 已实现」矛盾，
>   已改为「已实现」；
> - P1-1 二.8 `compat.allowedFallbackModels` 已实现（目录解析 + `fallbacks` 参数 +
>   `server-side-fallback` beta + 按返回模型计费 + 中途 fallback 防护）；
> - P1-2 二.2 `/bug` 本地导出已实现（`core/bug_report.rs` 脱敏收集/诊断/摘要请求
>   + `utils/zip.rs` 最小 ZIP + `core/crash_log.rs` panic 钩子与启动提醒
>   + `/bug` 四级面板与 worker 导出）。剩余未实现项减至 **6 项**（P2 3 项 / P3 3 项）。
> - 验证：`cargo test`（1669 单测 + 全部集成测试，共 1808 passed）与
>   `cargo clippy --all-targets` / `cargo fmt --check` 均通过。
>
> **第九轮（二.15 补全 + 二.1 落地 + 二.10 残留清理）**：
> - 二.15 的未移植部分（请求级上限 `inputLimits.maxRequestBytes` / `images.maxPerMessage` /
>   `images.maxPerRequest`）已实现——`ImageInputLimits`/`InputLimits` 解析 + `ModelEntry.input_limits`
>   + `modelOverrides.inputLimits` 深合并（pi `mergeInputLimits`）；与 pi 一致**只解析不强制**；
> - 二.1 Prompt cache warming 已实现（`core/cache_warmer.rs` + 设置/目录 TTL/成本评估/后台重放
>   + `cache_warming_decision` 扩展钩子 + `cache_warm` usage 记录 + `/session` 块与通知）；
> - 二.3 / 二.10 **经用户决定不实现**。复核发现二.10 描述的特性在 pi 0.86.0 已被删除并
>   并入二.3 的 transcript system-message 模型，二者需 canonical context（会话格式演进）才能落地；
> - **残留清理**：二.10（旧 deferred tool loading）在 prux 的残留——`AgentMessage.added_tool_names`
>   （pi `ToolResultMessage.addedToolNames`）、`ToolResult.added_tool_names`、`FinalizedToolCall.added_tool_names`
>   及工具结果 JSON 的 `addedToolNames` 字段——均为**只写不读**的死管道，已全部移除；
>   目录 `supportsToolSearch` 等 compat 元数据**保留**（pi 0.87.1 现行字段，仅元数据）。
> - 验证：`cargo test`（1687 单测 + 全部集成测试，共 1826 passed）与
>   `cargo clippy --all-targets` / `cargo fmt --check` 均通过（注：该套件在并行下偶发
>   `resolve_provider_model_falls_back_to_default` / `resume_selector_ctrl_jk_navigation_and_paste`
>   等与本次改动无关的 flake，在改动前的 HEAD 上同样复现）。
>
> **第十轮（未实现项定案 + 已知差异澄清）**：
> - 明确标为 **❌ 不实现**：二.3（用户决定）、二.10（上游已移除）、二.7（用户决定）、
>   二.4（用户决定）、二.16（已确认）、二.12 的 Muse 订阅 OAuth 刷新（用户决定）、
>   二.2 的失败回合「/bug 提示」行（用户决定）；
> - 澄清两项**真实差异**（原文档曾标 N-A，经复核不成立）：
>   一.2 被放弃 attempt 残留（error/overflow 的 assistant 条目已落库，
>   `build_context_messages` 只过滤 `deferred`，故 resume/树导航后仍会回到上下文）——
>   与二.13 绑定；二.1 的两个剩余差异（`cacheRetention` 作用域、`WarmStatus` 无时间点）——
>   判定为可接受的差异；
> - 细化二.13（canonical context）的缺失范围与落地成本，作为唯一「待议」的结构性缺口；
> - 修正第五节计数口径（此前「4 项」与实际列出的条目数不一致）。
>
> 本文档仅记录状态，未改动任何源码。
>
> **第十一轮（二.13 + 一.2 实现；二.1 完成）**：
> - 二.13 **已实现**：`Entry::ContextEdit`（append-only 上下文编辑，`replacement = null` 省略 /
>   非 null 只替换 content）+ `append_context_edit`（存在 / 活动分支 / 可编辑三项校验）+
>   `append_retain_none_compaction` + `build_context_messages` 投影应用（同一目标最新一条生效）
>   + 扩展边界草稿（`BoundaryOutcome.edits` / `.compactions`，`turn_end` 与 `agent_before_settle`
>   两个边界都消费，与 continue/end 决策无关）；
> - 一.2 **已修复**：恢复路径（溢出 / 瞬时错误重试）改为把被放弃的 attempt 落库为
>   `context_edit(replacement = null)`（`Agent::omit_entry_from_context`），
>   `/resume`、树导航、`--fork`、`/import` 后不再回到 provider 上下文，原始 transcript 保留；
> - 二.1 标记为**已完成**（两处与 pi 的差异判定为可接受，详见 二.1）；
> - 验证：`cargo test`（1700 单测 + 全部集成测试）与 `cargo clippy --all-targets` /
>   `cargo fmt --check` 均通过。

---

## 一、Bug 修复（需要评估是否移植）

### 1. 工具系统 (tools)

#### [0.86.0] `bash`/`powershell` 工具耗时格式（≥1 分钟按分秒）
- **问题**：工具耗时始终以秒显示。
- **修复**：≥60s 显示 `Xm Ys`，≥1h 显示 `Xh Ym Zs`（pi `renderers/bash.ts::formatDuration`；
  早期草稿写作 `m:ss`/`h:mm:ss` 与 pi 实现不符）。
- **prux 影响**：`src/modes/interactive/render/messages.rs` 目前固定 `Elapsed {:.1}s`（约 L1052）。
- **prux 状态**：✅ 已修复——新增 `format_tool_duration` 并替换 `Took`/`Elapsed` 三处；
  输出与 pi `formatDuration` 逐档一致（`59.9s` / `1m 0s` / `1h 2m 3s`），补阈值测试。

#### [0.86.0] 扩展工具缺少参数 schema 时应在注册期拒绝
- **问题**：无参数 schema 的扩展工具注册成功，直到 provider 请求才报错。
- **修复**：注册阶段即拒绝（#9300）。
- **prux 影响**：`src/core/extensions/tools.rs` 的 `ToolDef.parameters`（`Value`）目前无注册期校验。
- **prux 状态**：✅ N-A——prux 扩展为 Rust 内置，无动态注册路径（编译期/测试期即固定）。

#### [0.87.0] 以 `GIF` 开头的文本文件被误判为图片
- **问题**：`read` 工具和 CLI `@文件` 输入把魔数仅匹配 `GIF` 前缀的文本文件当成 GIF 图片，
  导致文本内容被丢弃（#9755）。
- **修复**：要求完整 GIF 签名（`GIF87a`/`GIF89a`）。
- **prux 影响**：`src/utils/mime.rs::detect_image_mime` 正是 `data.starts_with(b"GIF")`，
  与 pi 修复前完全一致，存在同一缺陷。
- **prux 状态**：✅ 已修复——改为校验 `GIF87a`/`GIF89a`，并补回归测试。

#### [0.87.0] 格式错误的 prompt 模板 frontmatter 被静默忽略
- **问题**：`.md` 模板 frontmatter 解析失败时无任何提示（#9830）。
- **修复**：作为 resource warning 上报。
- **prux 影响**：`src/core/prompt_templates.rs::parse_frontmatter` 解析失败直接返回 `None`，
  `load`（L181 调用处）无告警通道。
- **prux 状态**：✅ 已修复——新增 frontmatter 校验与 `take_load_warnings()`，启动/`/reload`
  以 Warning 提示并跳过该模板；补诊断测试。

### 2. 会话管理 (session)

#### [0.86.0] 精确 session ID 查找扫描完整 transcript 正文
- **问题**：按精确 ID 查找会话时读取整份 JSONL 正文而非只读 header，会话多时明显变慢（#9601）。
- **修复**：只读会话 header。
- **prux 影响**：`src/core/session_manager.rs` / `src/core/session_v4.rs` 的
  `JsonlV4Header` 已存在，但查找路径需确认是否只读 header。`src/cli/session_picker.rs`
  的 `find_session_by_id` 同样需核对。
- **prux 状态**：✅ 已修复——`session_picker::find_session_by_id` 仅读 JSONL 首行 header（补测试）。

#### [0.86.0] session tree 导航与进行中的 compaction 竞争
- **问题**：树导航会替换 compaction 进度 UI 并产生竞争（#9179）。
- **修复**：导航与 compaction 互斥/协调。
- **prux 影响**：prux `handlers/sessions.rs` 有 `/resume` 选择器与 `--fork`，
  `src/core/agent_session.rs` 有手动/阈值压缩。需确认二者在 prux 的 worker 串行模型下是否仍有竞争。
- **prux 状态**：✅ N-A——worker 串行处理命令，导航与 compaction 不会并发。

#### [0.87.0] 错误重试 / 最终 length·overflow 恢复把被放弃的 attempt 留在后续上下文
- **问题**：被放弃的模型尝试残留在后续 provider 上下文中；恢复期的省略未持久化。
- **修复**：post-run 恢复省略持久化，但不隐藏原始 transcript、不改变队列调度。
- **prux 影响**：`src/core/agent_session.rs` 的 overflow/length 恢复（`recover_from_overflow`、
  `overflow_recovery_attempted`，约 L1380–1500）与重试路径。
- **prux 状态**：✅ **已修复（第十一轮）**——落地了二.13 的 `ContextEditEntry` 后，恢复路径改为
  **持久化省略**：
  - `Agent::omit_entry_from_context`（`core/agent_session.rs`）→
    `Session::append_context_edit(target_id, None)`：追加一条 append-only 的 `context_edit`
    （`replacement: null`），**不改写历史**；
  - 调用点：`recover_from_overflow`（溢出 attempt）与 `retry_transient_errors`（瞬时错误重试的
    每次被放弃 attempt），与被放弃消息的 `entry_id` 绑定（无 session / 无 entry id 时静默跳过）；
  - `build_context_messages` 应用编辑：`/resume`、树导航、`--fork`、`/import` 之后
    被放弃的 attempt **不再回到 provider 上下文**，而原始 transcript（UI / `/bug` / 树）完整保留；
  - 附带修正：`reduce_lane_state` 的「最新 own 条目」跳过 `context_edit`，
    避免一条恢复省略顶掉 `terminal_failure` / `deferred` 判定。
  测试：`retry_omits_abandoned_attempt_from_rebuilt_context`、
  `overflow_recovery_omits_failed_attempt_persistently`、
  `terminal_failure_survives_trailing_context_edit`。

### 3. 压缩 (compaction)

#### [0.86.0] 运行中阈值压缩跳过超大尾随工具结果
- **问题**：mid-run 阈值自动压缩会静默跳过超大的尾随工具结果（#9740）。
- **修复**：将超大尾随工具结果纳入压缩输入。
- **prux 影响**：`src/core/compaction/engine.rs` 已有 `trailing_tokens` 估算与
  `context_tokens > context_window - reserve_tokens` 判定（L121–164）。
- **prux 状态**：✅ 已修复——抽出 `find_compaction_cut_index`，tool result 计入保留预算并回退到本轮起点（#9740）。

#### [0.86.0] 取消期间的竞争会误启动自动压缩 / 留下陈旧 retry 状态 / 漏掉摘要鉴权等待
- **问题**：取消竞争（#9340、#9777）。
- **修复**：统一取消传播，覆盖自动压缩启动、retry 状态、summarization 鉴权等待。
- **prux 影响**：`src/core/agent_session.rs`（压缩触发与 abort/取消）+ `src/core/compaction.rs`。
  当前在 `engine.rs`/`compaction.rs` 中未见取消语义。
- **prux 状态**：✅ N-A——worker 独占 Agent + drop-future 取消；无 pi 式异步会话竞争。

#### [0.87.1] 分轮（split-turn）压缩摘要被 Claude Fable 5.1 拒绝
- **问题**：split-turn compaction 摘要因对话/续写边界不清被 Fable 5.1 拒绝（#9908）。
- **修复**：明确分隔对话并使用续写导向指令。
- **prux 影响**：`src/core/compaction.rs` / `src/core/compaction/engine.rs` 的主摘要 prompt
  （`SUMMARIZATION_PROMPT`）。
- **prux 状态**：✅ N-A——prux 压缩保留整轮、不做 split-turn，无 prefix prompt。

### 4. Provider / 流式处理

> prux 有统一的 provider 层（`anthropic.rs`/`codex.rs`/`completions.rs`/`responses.rs`/`google.rs`）
> 与 32 个内置 provider 目录，以下修复按「prux 有对应 provider」收录。

#### [0.86.0] 无 body 的 HTTP 400/413 被误判为 context overflow
- **问题**：非 Cerebras provider 返回无 body 的 400/413 时被误判为上下文溢出（#9482）。
- **修复**：无 body 时不做 overflow 归类。
- **prux 影响**：`src/core/agent_session.rs::is_overflow_error_str`（L2268，substring 近似
  pi `overflow.ts`）。
- **prux 状态**：✅ 已实现——仅 `provider=="cerebras"` 的无 body 400/413 计为溢出（`is_bodyless_status_error`，provider 门控）。

#### [0.86.0] z.ai `Prompt too long` 未识别为上下文溢出
- **问题**：z.ai 的溢出文案未命中 overflow 模式（#9805）。
- **修复**：纳入识别。
- **prux 影响**：同上 `is_overflow_error_str`；prux 有 `zai`/`zai-coding-cn` provider。
- **prux 状态**：✅ 已对齐——`is_overflow_error_str` 已含 `prompt too long`。

#### [0.86.0] Cerebras strict tool schemas
- **问题**：Cerebras 模型声明了不支持的 strict tool schema，strict 与非 strict 混用时 HTTP 400（#9804）。
- **修复**：Cerebras 不启用 strict。
- **prux 影响**：`src/core/model_resolver.rs`（provider 能力探测/`compat`）+
  `src/core/tools/index.rs::experimental_tool_sampling`。
- **prux 状态**：✅ 已对齐——`supports_strict_mode` 默认 false，已排除。

#### [0.86.0] GitHub Copilot GPT 模型用了 Chat Completions 而非 Responses
- **问题**：Copilot 的 GPT 系列（含 GPT-6 Astra）走了错误的 adapter（#9253）。
- **修复**：改用 Responses adapter。
- **prux 影响**：`src/core/model_resolver.rs` 的 Copilot adapter 分发 + `src/core/provider/responses.rs`。
- **prux 状态**：✅ 已修复——`models.github-copilot.json` 中 `gpt-6-astra` 的 `api` 改为 `openai-responses`。

#### [0.86.0] DeepSeek V4.1 thinking level 元数据在 OpenRouter / OpenCode Go 上丢失
- **修复**：保留 provider effort 元数据（#9485）。
- **prux 影响**：`src/core/model_resolver.rs`、`src/core/provider/completions.rs`。
- **prux 状态**：✅ 已对齐（运行期）——prux 运行期读取目录 `thinkingLevelMap` 并透传 effort；pi 该修复在目录生成脚本，目录数据细节属全量 `sync-models.py` 范畴（见 #17）。

#### [0.86.0] OpenAI Codex 未发送 Off reasoning effort
- **问题**：Codex 请求省略 Off effort，未尊重不支持 Off 的映射（#9191）。
- **修复**：正确发送 Off，并处理不支持情形。
- **prux 影响**：`src/core/provider/codex.rs`。
- **prux 状态**：✅ 已修复——`codex.rs` 按 pi 映射规则重写（off→省略 / "none" / 映射字符串）。

#### [0.86.0] Fireworks unsigned thinking replay 与 effort 选择
- **修复**：按目录元数据重放 unsigned thinking 并选择 reasoning effort（#9323）。
- **prux 影响**：`src/core/provider/completions.rs`（Fireworks 走 completions）+ 目录 `models.fireworks.json`。
- **prux 状态**：✅ 已实现——`anthropic.rs` 新增 `compat_allow_empty_signature`：目录 `compat.allowEmptySignature=true` 的模型（Fireworks/Vercel）保留 thinking 块 + 空 signature，其余仍降级为文本；补测试。目录侧 `compat`/`thinkingLevelMap` 已随全量同步对齐 pi（校验 0 diff）。

#### [0.86.0] Vercel AI Gateway 把未签名 thinking 当 assistant 文本重放
- **修复**：#9676。
- **prux 影响**：`src/core/provider/completions.rs`；prux 有 `vercel-ai-gateway`。
- **prux 状态**：✅ 已实现——同 #9323；全部 Vercel 条目 `compat.allowEmptySignature=true` 已生效。

#### [0.86.0] Anthropic 兼容 relay：响应 model 与请求不一致时 signed thinking 重放失败
- **修复**：以返回 model 判断重放/定价（#9188）。
- **prux 影响**：`src/core/provider/anthropic.rs`。
- **prux 状态**：✅ 已对齐——重放按请求 model；prux 无 fallback 定价特性。

#### [0.86.0] Google / Vertex：reasoning 省略或同族能力不一致时使用不支持的 thinking level
- **修复**：#9455。
- **prux 影响**：`src/core/provider/google.rs`。
- **prux 状态**：✅ 已实现——`google.rs` 重写：`uses_google_thinking_level`（Gemini 3 Pro/Flash、flash-latest/-lite、Gemma 4）、`resolve_google_thinking_level`（映射优先，不再按模型族改判）、`disabled_thinking_config` 改用 `clamp_thinking_level("off")` 取最低受支持级（Gemma 4 旧实现错发 `thinkingBudget:0`）；补 3 个测试。

#### [0.86.0] 排空缓冲 `EventStream` 事件时 CPU 二次方增长
- **修复**：#9055（大批量流式事件时的性能问题）。
- **prux 影响**：各 provider 的 SSE 累积器（`completions.rs`/`codex.rs`/`responses.rs`）。
- **prux 状态**：✅ N-A——无事件队列（同步回调），SSE buffer 有界。

#### [0.86.0] OpenAI-compatible Responses 错误总被标成 OpenAI
- **修复**：标注真实 provider（#9298）。
- **prux 影响**：`src/core/provider/responses.rs` 的错误构造。
- **prux 状态**：✅ 已对齐——prux 无该前缀，本无此缺陷。

#### [0.86.0] 重试分类：Cloudflare 520
- **修复**：#9627，纳入可重试。
- **prux 影响**：`src/core/provider/retry.rs::should_retry`（当前 408/409/429/≥500）。
- **prux 状态**：✅ 已修复——agent 级 `RETRYABLE` 增补 `"520"`（补测试）。

#### [0.86.0] 重试分类：Azure 峰值容量瞬时错误
- **修复**：#9669。
- **prux 影响**：同上 `retry.rs`。
- **prux 状态**：✅ 已修复——增补 `"currently experiencing high demand"`（补测试）。

#### [0.86.0] agent 级重试退避上限 `retry.maxAgentDelayMs`（默认 60s）
- **问题**：长时间瞬时故障下 agent 级重试退避失控（#8826）。
- **修复**：给 agent 级退避设上限。
- **prux 影响**：`src/core/provider/retry.rs` 已对**发送层**设 60s 上限；
  但 prux 是否有 agent 级退避循环需确认。
- **prux 状态**：✅ 已修复——`MAX_AGENT_RETRY_DELAY_MS=60s` 常量封顶（无新配置键，send 层原已有 60s 上限）。

#### [0.87.0] 未知 OpenAI-compatible 端点不应收到 strict tool schema
- **问题**：未声明支持 strict 的端点收到 strict schema（#9816）。
- **修复**：仅对显式声明支持的内置模型启用 strict。
- **prux 影响**：`src/core/provider/completions.rs` + `src/core/model_resolver.rs` 的 strict 判定。
- **prux 状态**：✅ 已对齐——默认非 strict（比 pi 更保守）。

#### [0.87.1] 纯图片 user 消息带空 text part，被部分 OpenAI-compatible provider 拒绝
- **问题**：多模态 user 消息即使只有图片也带一个空 `text` part（#9797）。
- **修复**：过滤空 text part；全空则跳过整条消息。
- **prux 影响**：`src/core/provider/completions.rs::convert_user_message`（L91）
  对 `ContentBlock::Text` **无条件** push `{"type":"text",...}`，与 pi 修复前一致，存在同一缺陷。
- **prux 状态**：✅ 已修复——push 前过滤空 text；补回归测试。

#### [0.87.1] Anthropic OAuth 上报过期的 Claude Code 版本
- **问题**：`claude-cli/<version>` 版本过旧。
- **修复**：pi 由 `2.1.251` → `2.1.280`。
- **prux 影响**：`src/core/provider/anthropic.rs:1294` 硬编码 `claude-cli/2.1.75`，比 pi 基线更旧。
- **prux 状态**：✅ 已修复——抽为 `CLAUDE_CODE_VERSION` 常量并更新到 2.1.280。

### 5. TUI / 渲染

#### [0.86.0] skill 斜杠命令补全按 `skill:` 前缀而非裸名排序
- **问题**：补全把 `skill:` 前缀当作匹配文本，排序错误（#9120）。
- **prux 影响**：prux 有 `/skill:name` 命令候选（`app.rs` 注释、`handlers/suggest.rs`）。
- **prux 状态**：✅ 已修复——`suggest.rs` 去 `skill:` 前缀参与模糊匹配与排序（补测试）。

#### [0.86.0] 文件补全在 CJK 标点处的边界与路径引号
- **修复**：#9746。
- **prux 影响**：`src/modes/interactive/render/suggestion.rs` + `handlers/suggest.rs`。
- **prux 状态**：✅ 已实现——分隔符含 CJK 标点；含分隔符的路径补全加引号（补测试）。

#### [0.86.0] LaTeX：legacy 字体切换回退原文、`cases` 居中对齐、嵌套/不支持的 display 上下标垂直排版
- **修复**：#8827、#9564、#7929。
- **prux 影响**：`src/modes/interactive/render/latex.rs`（移植自 pi `latex.ts`）。
- **prux 状态**：✅ 已实现——字体切换（`FONT_SWITCH_COMMANDS`）；新增 `LayoutNode::Script` 垂直堆叠嵌套/不支持 Unicode 的 display 脚本（#7929）；`cases` 改为居中矩阵节点 + 偶数行插入纯分隔符行（#9564）；输出与 pi 测试字符串逐字对齐；补 2 个测试。

#### [0.86.0] 剪贴板：本地失败应回退 OSC 52；容器/WSL 无显示时恢复 OSC 52
- **问题**：本地剪贴板失败时报成功；无显示环境无 OSC 52 回退（#9688、#9618）。
- **修复**：本地优先 + OSC 52 兜底，失败时给出可操作提示。
- **prux 影响**：`src/utils/clipboard.rs` 已是「本地优先 + OSC52 垫底」，但失败上报/提示需核对。
- **prux 状态**：✅ 已实现——仅远程/无显示 Linux 用 OSC52；OSC52 有大小上限；失败返回平台指引（补测试）。

#### [0.86.0] 模糊搜索长文本性能
- **修复**：用原生子串搜索替代逐字符扫描（#9267，JS 侧）。
- **prux 影响**：`src/utils/fuzzy.rs` 为 Rust 实现。
- **prux 状态**：❌ 结构性 N/A（Rust 实现无 JS 逐字符开销）；可选做基准确认。

#### [0.86.0] 全屏模式下渲染 0 行的自定义 footer 仍占一空行
- **修复**：#8919。
- **prux 影响**：prux 无 pi 式「全屏 transcript」模式（`OverlaySize::Fullscreen` 是覆盖层，不是同一特性）。
- **prux 状态**：❌ 结构性 N/A。

### 6. 扩展系统 (extensions)

#### [0.87.0] `context` 处理器过滤/切片 messages 时丢失 prompt 与工具声明
- **问题**：扩展在 context 阶段返回过滤后的 messages，会丢掉系统提示与工具声明，
  导致请求没有内置工具、或 Codex 输出原始 tool-call 文本；处理器还会看到 system 消息（#9789、#9822）。
- **修复**：处理器不再看到 system 消息；运行后由 Pi 恢复 prompt 与工具状态。
- **prux 影响**：`src/core/extensions/hooks.rs::ExtensionHook::TransformContext` +
  `src/core/agent_session.rs` 的上下文构造。
- **prux 状态**：✅ N-A——system prompt 独立传入，`agent.messages` 无 system 消息，无丢失风险。

#### [0.86.0] 扩展工具无参数 schema 注册期拒绝
- 见「工具系统」同名条目。

### 7. OAuth / 认证

#### [0.87.1] Anthropic OAuth 版本
- 见「Provider / 流式处理」同名条目。

#### [0.86.0] 登录后过早报 missing model（等待目录发现）
- **问题**：登录后目录尚未发现即报模型缺失（Radius 场景）。
- **修复**：等待目录发现。
- **prux 影响**：`src/core/model_refresh.rs`、`src/core/login_registry.rs`、`src/cli/auth_command.rs`。
- **prux 状态**：✅ N-A——prux 无 Radius；内嵌目录登录后即可用。

---

## 二、新增功能（需要评估是否移植）

### 1. [0.86.0] Prompt cache warming（提示缓存保温）
- **描述**：长工具运行期间/空闲时按成本感知刷新 prompt cache；含模式配置、模型 cache 生命周期
  元数据、`/session` 诊断、transcript 通知与 `cache_warming_decision` 扩展事件。
- **prux 可用性**：prux 无缓存保温（仅有 codex 的 `prompt_cache_key`）。
- **建议**：中优先级；成本收益取决于用量。
- **prux 状态**：✅ **已完成（第九轮实现，第十一轮确认收尾）**：
  - `core/cache_warmer.rs`：`CacheWarmingMode`（`off`/`streaming`/`idle`，缺失默认 `streaming`，
    **只读全局** settings）+ `PRUX_CACHE_RETENTION`（short/long/none，与 pi `PI_CACHE_RETENTION` 同义）
    + 目录 `promptCache` TTL 运行期解析 + 刷新延迟（TTL×0.9，≥10s 裕量）+ 硬上限
    （streaming 1h / idle 30min）+ 刷新截止点（延迟 + 剩余裕量一半）+ 成本评估
    （`compute_cost` 定价：miss = cacheWrite|input − cacheRead，warm = cacheRead + 1 output token；
    idle 续跑概率 0.15；期望节省 ≥ $0.05）+ 可重放性判定（预算式 thinking 的 anthropic 请求除外，
    adaptive thinking 例外）+ 后台 tokio 任务（延迟→评估→`stream_chat(max_tokens=1)`→回执→重排）；
  - `cache_warming_decision` 扩展钩子（`ExtensionHook::CacheWarmingDecision`，最后一个给出
    action 的扩展生效；事件负载 `warmCost`/`missCost`/`continuationProbability`/`action`）；
  - 接线：`agent_loop::perform_llm_call` 在非 error/aborted 的会话请求后启动（子代理/离线不启动，
    `agent_settled` 时 `streaming` 停 / `idle` 转段）；worker 回合结束与空闲 1s tick 应用回执
    （落 `cause = "cache_warm"` usage 记录并计入会话统计 + 发 `cache_warm` 事件）；
  - 用户可见：`/session` 的 `Cache Warming` 块（Mode/Status/Cache miss penalty/Refresh cost/Warmed）、
    settings 面板 `Cache warming` 项、`showCacheMissNotices` 开启时的 `Cache warmed: $<cost>` 通知。
  - **已核实的两处与 pi 的差异（判定为可接受，不移植）**：
    ① `cacheRetention` 的**作用域**：pi 是请求级选项（调用方/扩展可对单次请求指定
      `off`/`short`/`long`），prux 只有**进程级** `PRUX_CACHE_RETENTION` 环境变量
      （`cache_warmer::read_cache_retention`，无 settings 键、无按会话/按请求覆盖）——
      影响面：同一进程内切换档位需重启；
    ② `/session` 状态无时间点：`WarmStatus`（字段 `mode`/`phase`/`detail`/`economics`/
      `last_warmed_cost`/`total_warmed_cost`/`warmed_count`）**不带时间戳**；
      `detail` 里的延迟值取自 `start()` 时的 TTL，而 `run_warmer` 内部每次刷新都重算成本评估
      （决策逻辑与 pi 一致，只是展示用启动值）。

### 2. [0.86.0] Bug reporting（`/bug`）
- **描述**：`/bug [description]` 收集脱敏环境/模型/provider/扩展/设置元数据、会话诊断，
  可选 transcript 或模型摘要，上传 Radius 或导出 zip；崩溃写入 `~/.pi/agent/crashes.json`。
  0.87.0 补充：离线模式禁止上传但保留本地 zip 导出（#9841）。
- **prux 可用性**：此前无 `/bug`、无 crash log。
- **建议**：拆成两半——
  - **本地部分（可独立做，不依赖 Radius）**：脱敏收集环境/模型/provider/扩展/设置 + 会话诊断，
    可选 transcript 或模型摘要，导出 zip；崩溃写 `~/.prux/crashes.json`；
  - **上传部分**：prux 无 Radius，不做（对应 pi 0.87.0 #9841 的「离线禁止上传、保留本地 zip」语义）。
- **prux 状态**：✅ 已实现（本地导出部分）：
  - `core/bug_report.rs`：脱敏（敏感键值 → `<redacted>`，URL userinfo / 敏感查询参数清洗）
    + 元数据（环境/终端/`PRUX_*` 变量名、会话、模型/provider、扩展、全局+项目设置）
    + 诊断（失败/中止的 assistant 轮次 + 崩溃记录，不含对话正文）
    + 分支 transcript（`Session::serialize_branch_jsonl`：header + 分支 mutation，可 `/import` 还原）
    + 摘要请求构造（上下文窗口 60% 预算 + pi 同款 prompt）；`utils/zip.rs` 为 stored-only 最小 ZIP 写入；
  - `core/crash_log.rs`：panic 钩子写 `agent_dir()/crashes.json`（最多 5 条、7 天内提示一次），
    启动时提醒「Run /bug to report it; the crash details are attached automatically.」，成功导出后清空；
  - TUI：`/bug [description]` 四级面板（描述 → 是否附带 transcript → 是否生成摘要 → Export as Zip/Cancel），
    worker 侧 `AgentCommand::BugReport` 组装并写 `prux-bug-report-<id>.zip`，同时在会话内记一条
    `prux.bug-report` 自定义条目；
  - 上传部分 ❌ **不实现**（prux 无 Radius，对应 pi 0.87.0 #9841 的「离线禁止上传、保留本地 zip」语义）；
  - 失败回合后的「/bug 提示」行（pi `maybeSuggestBugReport`）❌ **不实现（用户决定）**：
    prux 已在启动时由 `core/crash_log.rs` 提醒真实 panic；而「上一轮出错就提示报 bug」
    在 prux 里价值有限（错误信息本身已在 transcript 可见），且每次可重试错误后重复提示会打扰，
    故不移植。

### 3. [0.86.0] Transcript-aware prompt/tool updates（会话内系统提示与工具变更）
- **描述**：中途修改系统提示/工具集后，经 transcript 持久化，在 resume 与分支导航后仍生效，
  并在支持的模型上保留缓存前缀。
- **prux 可用性**：prux 有 `core/system_prompt.rs`，但无 transcript 持久化式的中途变更。
- **建议**：中优先级，与 0.87.0 的 canonical context 一起评估。

### 4. [0.86.0] Offline Radius model catalog
- **描述**：内置 Radius 目录，支持离线/即时模型选择，并叠加缓存与实时网关目录。
- **prux 可用性**：prux 无 Radius provider。
- **建议**：低优先级（除非需要 Radius）。
- **prux 状态**：❌ **不实现（用户决定）**。理由：
  - 前置依赖是 `pi-messages` 网关 provider 本体（API + OAuth 登录 + 计费/用量上报），
    内嵌目录只是其附属；`login_registry.rs` 已标注 Radius「暂不实现」；
  - 该特性的收益（首启/离线即可选模型、少一次网络发现）对 prux **不成立**——
    prux 的目录本来就是内嵌的（`assets/models/*.json`），离线即可选模型。

### 5. [0.86.0] Per-model compaction budgets
- **描述**：`compaction.modelOverrides` 按模型设置 `reserveTokens`/`keepRecentTokens`，
  回退到全局设置。
- **prux 可用性**：`src/core/compaction/engine.rs` 仅有**全局** `reserve_tokens`/`keep_recent_tokens`
  （来自 settings），无按模型覆盖。
- **建议**：高优先级，改动小、收益明确。
- **prux 状态**：✅ 已实现——新增 `compaction.modelOverrides` 读取（`provider/modelId` 键，
  兼容 pi 嵌套全局键）+ `CompactionSettings::for_model`，在 `maybe_compact_inner` 生效；补测试。

### 6. [0.86.0] 点击切换 branch/compaction 摘要与 skill 调用条目
- **描述**：鼠标点击折叠/展开上述条目（pi 侧覆盖 branch summary、compaction summary、
  skill invocation、thinking 块、tool execution，均由各自 `MouseRegion` 切 `expanded`）。
- **prux 可用性**：prux 有鼠标子系统（`handlers/mouse.rs`：滚轮/拖选/滚动条/中键粘贴），
  但**无 per-entry 展开状态**——branch/compaction summary、skill 块、tool 输出全部由
  单个全局 `App::expand_all`（Ctrl+O）驱动（`render/messages.rs:1280` `render_summary` 等）。
- **建议**：✅ **已实现**（用户拍板；手势定为 **Alt+左键**，裸左键留给文本选择）。prux 块模型：
  `BlockKind{Summary,Skill,Thinking,Tool}` + `BlockId{ts,seq,block}`（消息内块序号，顺序 = 渲染顺序）；
  渲染时收集 `BlockSpan`（存入 `CachedMsg`）→ 内容全局行命中区（`ClickTarget` → `MouseSel::click_targets`，
  每帧重建并同期绑到屏幕行）；`handle_mouse_left_down` 先做命中反查，命中则单独切换并只失效该条目缓存。
  逐块覆盖存 `App::expand_overrides: HashMap<BlockId,bool>`，缺省回退全局：
  thinking → `show_thinking`，摘要/skill/工具块 → `expand_all`。
  **全局开关按 pi 语义覆盖逐块态**：`toggle_expand_all`（Ctrl+O）丢弃所有非 thinking 块的逐块覆盖
  （pi `setToolsExpanded` 把新值 `setExpanded` 到每个组件），`toggle_show_thinking`（Ctrl+T）
  清空 thinking 的逐块覆盖（pi `setHideThinkingBlock` → `thinkingVisibilityOverrides.clear()`），
  两个开关互不干扰；值不变时早退、不清覆盖。
  范围与 pi 对齐：branch/compaction 摘要、skill 调用、**thinking run**、**工具调用块**（含内联输出）。
  会话切换/树导航/`/new` 清空逐块展开态；后台补高亮快照携带 `overrides`（否则补高亮后逐块态丢失）。
  测试：`collapsible_summary_registers_click_target_and_expands_per_item`、`skill_user_message_is_collapsible`、
  `assistant_thinking_and_tool_blocks_toggle_independently`、
  `global_toggles_overwrite_per_block_overrides_like_pi`、`alt_left_click_toggles_collapsible_summary`、
  `plain_left_click_on_collapsible_keeps_selection_behaviour`。

### 7. [0.86.0] `ctx.modelRegistry.stream()` / `streamSimple()`
- **描述**：扩展通过已配置 provider 与已解析鉴权发起模型调用。
- **prux 可用性**：prux 扩展为 Rust 内置（`core/extensions`），无等价公开 API。
- **建议**：低优先级（按需）。
- **prux 状态**：❌ **不实现（用户决定）**。理由：
  - prux 扩展是**编译进二进制的 Rust trait**（`core/extensions`），没有 pi 那种可下发
    任意 JS 的公开宿主 API；向外部扩展开放「用 prux 已解析的凭据代发模型请求」会引入
    凭据外泄面；
  - 等价诉求已由 `src/extensions/proxy/`（OpenAI 兼容 proxy，外部客户端可走 prux 的
    provider）覆盖大半；prux 内部要发模型请求可直接调 `core::provider::*`，无需新公开 API。

### 8. [0.86.0] `compat.allowedFallbackModels`
- **描述**：覆盖/禁用 Anthropic 服务端 fallback 模型。
- **prux 可用性**：此前无该配置；但目录数据**已在**
  （`models.anthropic.json` 中 `claude-fable-5` 含 2 条 fallback）。
- **建议**：低→中优先级（数据就绪，改动小）。
- **prux 状态**：✅ 已实现：
  - 目录 → `ModelEntry`/`ModelConfig` 新增 `allowed_fallback_models`（`AllowedFallbackModel{provider,model,cost}`，
    条目缺 `model` 时丢弃）；
  - `anthropic.rs`：非空时请求带 `fallbacks: [{model}]` + `server-side-fallback-2026-07-01` beta；
  - `message_start` 返回的 `message.model` 与请求模型不同时记为 `responseModel`，
    并按目录中匹配（provider + model）条目的 `cost` 计费（`billing_cost`，`message_start`/`message_delta` 两处）；
  - 输出中途出现的 `fallback` content block：流开头（尚未输出任何块）忽略，已有输出则报
    `unsupported mid-output model fallback` 终止本轮（pi 同款）。

### 9. [0.86.0] 内置工具默认启用 strict-prefer JSON-schema 采样
- **描述**：`read`/`bash`/`powershell`/`edit`/`write` 默认启用 strict-prefer，
  不再需要 `PI_EXPERIMENTAL`；扩展可用 `constrainedSampling: false` 退出。
- **prux 可用性**：prux 的 `constrained_sampling` 由 `PRUX_EXPERIMENTAL`
  （`src/core/tools/index.rs::experimental_tool_sampling`）门控，默认关闭。
- **建议**：高优先级——默认开启并让扩展工具可显式关闭。
- **prux 状态**：✅ 已实现——`read`/`bash`/`powershell`/`edit`/`write` 默认 `constrained_sampling=true`，
  移除 `experimental_tool_sampling`；扩展仍可用 `constrained_sampling=false` 覆盖。

### 10. [0.86.0] Fireworks Messages 原生 deferred tool loading
- **描述**：以 `ToolSearch`/`tool_search` 作为 loader 名做 prompt 前缀延迟加载。
- **prux 可用性**：prux 有 `fireworks` provider 但无 deferred tool loading；
  目录 17 处 `supportsToolSearch`（`openai`/`openai-codex`）未被使用。
- **建议**：低优先级。

### 11. [0.86.0] `--resume` 渐进式加载 / `--continue` 启动加速
- **描述**：`--resume` 结果渐进出现、按 mtime 优先、选中后取消未完成读取；`--continue` 按 mtime 检查候选并提前停止。
- **prux 可用性**：`src/cli/session_picker.rs` 有会话选择器。
- **prux 状态**：✅ 已实现（三处）：
  - `--resume` 选择器（`cli/session_picker.rs`）：新增 `ProgressiveMetas` 渐进加载器——候选按
    mtime 新→旧，后台线程逐条读展示元数据（名称/首条消息/消息数/相对时间）经 channel 回流，
    选择器每 50ms 取回并重绘（"loading n/m"），**选中/取消即 cancel**（后台在下一个候选前停止）。
    此前只显示 UUID 文件名，且要先扫完整个目录；
  - TUI `/resume` 选择器（`modes/interactive/session_selector.rs`）：候选枚举改为**只 stat 按
    mtime 排序**（`candidate_paths` / `all_candidate_jobs`），`open()` 只同步加载首批
    `INITIAL_BATCH=12` 行出首帧，其余进入 `pending` 由事件循环 80ms tick 每帧补齐
    `DRAIN_BATCH=16` 行（`drain_pending`，选中项按路径保持）；`close()`/选中即
    `cancel_pending()`；header 显示 `Loading n/m…`，无结果时提示"Loading sessions…"。
    all scope 同样按全局 mtime 优先（对齐 pi all-folder 优先顺序）；
  - `--continue`（`session_manager::continue_recent_checked`）：显式 `--session-dir` 且非本 cwd
    默认目录时**先读首行 header 的 cwd 过滤**（对齐 pi `findMostRecentSession(dir, cwd)` 的
    提前停止），只对 cwd 匹配的候选做完整打开——顺带修掉"`--continue --session-dir X` 会
    恢复别的项目会话、并把它的空会话当空会话删除"的跨项目 bug。
  测试：`session_picker` 4 项（mtime 顺序/渐进性/取消/坏文件跳过）、
  `progressive_loading_fills_rows_in_mtime_order`、
  `continue_recent_filters_other_cwd_sessions_in_explicit_dir`。

### 12. [0.86.1] Meta（Muse 订阅）provider
- **描述**：`/login meta` 或 `META_API_KEY` 访问 Muse Spark 模型，自动刷新 Model API key。
- **prux 可用性**：prux **已实现 API key 路径**——目录 `assets/models/models.meta.json`
  （5 个 Muse Spark 模型，`api: openai-responses`，`baseUrl: https://api.meta.ai/v1`）、
  `login_registry` 的 `meta` 登录项（`supports_api_key: true`）、`auth.rs:320` 的
  `META_API_KEY` 映射、`model_resolver` 的 `SUPPORTED_PROVIDERS`/`baseline_models`
  （默认 `muse-spark-1.3`）。
- **prux 状态**：✅ 已实现（API key 路径）。**唯一缺口**：pi 另有 Muse 订阅 OAuth
  （`isSubscription: true` + `loadMetaOAuth`，自动刷新 Model API key），prux 未移植——
  只有该 OAuth 刷新算剩余工作。
  **决定**：该 OAuth 刷新 ❌ **不实现（用户决定）**——需要实现 Meta 订阅的 OAuth
  设备码/刷新流程并管理**自动轮换的 Model API key**（prux 无此凭据类型，
  `oauth.rs` 的既有 provider 均为固定 client 的授权码/设备码流程），
  而 API key 路径已能完整使用 Muse Spark 模型。

### 13. [0.87.0] Canonical session context 与扩展边界（`ContextEditEntry`）
- **描述**：以 append-only 的 context edit 在不改写历史的前提下编辑模型上下文
  （`appendContextEdit(entryId, null)` 省略某条消息）；新增可执行的 `turn_end` 与
  `agent_before_settle` 边界；`retain-none` 压缩输入（`appendCompaction(summary, null, tokensBefore)`）。
- **prux 可用性**：prux `session_v4.rs` 的 `Entry` 并集无 `context_edit`；
  边界（`turn_end` / `agent_before_settle`）**已实现**（prux 形态，见四.4），
  仅 `ContextEditEntry` 与 `retain-none` 压缩输入未做。
- **prux 状态**：✅ **已实现（第十一轮）**——`ContextEditEntry` 与 retain-none 压缩输入均已落地：
  - **数据模型**（`core/session_v4.rs`）：`Entry::ContextEdit { targetId, replacement }`
    （`replacement = null` 省略 / 非 null 只替换 content），配套 `ContextEditReplacement`；
    枚举穷举点（`id`/`seq`/`entry_base`/`entry_base_mut`/`set_entry_base`/`entry_type_name`）、
    `validate_entry_payload`（空 targetId 拒绝）与 `terminal_failure` 判定均已同步；
  - **append API**（`core/session_manager.rs`）：`append_context_edit(target_id, replacement)`——
    与 pi 一致做三项校验（目标存在 / 在活动分支上 / 是可编辑的 user·assistant·toolResult 消息），
    违反即 `Err` 不写盘；`append_retain_none_compaction(summary, tokens_before, …)`
    （retain-none：`firstKeptEntryId` 自引用，保留区间为空）；
  - **投影**（`build_context_messages`）：先收集「本次上下文条目集合」内的编辑（同一 target
    以**最新**一条为准），再逐条应用——`null` 跳过该条目、非 null 只换 `content`（保留 role /
    provider / model / stopReason / toolCallId）；`context_edit` 自身不产生消息；
    被压缩摘要吞掉的目标不受影响（与 pi `buildSessionProjection` 的 edits map 同义）；
  - **扩展边界草稿**（`BoundaryOutcome`）：新增 `edits: Vec<ContextEditDraft>`
    （`with_omit` / `with_content_replace`）与 `compactions: Vec<BoundaryCompaction>`
    （`with_retain_none_compaction`）；`turn_end` 与 `agent_before_settle` 两个边界都消费，
    草稿**先落库并立即重建上下文**，与 `continue` / `end` 决策无关（error / aborted 轮同样生效，
    pi `_commitBoundaryDrafts` + `_refreshFinalizedContext` 同款）；
  - **内置消费者**：一.2 的恢复省略（见一.2）；树渲染把该条目显示为 `ctx_omit/<targetId>`
    （`worker.rs::session_tree_text`，对齐 pi `[context omit: id]`）。
  - 与 pi 的差异：prux 的消息内容统一是 `Vec<ContentBlock>`，故 `replacement.content` 不接受
    pi 的「字符串」形态（只接受内容块数组）；`custom_message` 条目类型在 prux 不存在。
  测试：`context_edit_entry_roundtrips_and_validates`（解析/校验/往返）、
  `context_edit_omits_target_from_rebuilt_context_and_persists`、
  `context_edit_replaces_content_and_latest_edit_wins`、
  `context_edit_before_latest_compaction_has_no_effect`、
  `context_edit_rejects_missing_off_branch_and_non_editable_targets`、
  `retain_none_compaction_keeps_only_summary`、
  `turn_end_boundary_context_edit_omits_entry`、
  `turn_end_boundary_retain_none_compaction_self_retains`。

### 14. [0.87.0] Full-transcript context 扩展（`context_with_system`）
- **描述**：在 `context` 处理器之后、对含 system 消息的完整 transcript 运行，结果原样发送。
- **prux 可用性**：prux 仅有 `TransformContext`（无 system 消息版本）。
- **prux 状态**：✅ 已实现：
  - 新 hook `ExtensionHook::ContextWithSystem` + `Extension::transform_context_with_system(&mut Vec<AgentMessage>)`
    + `extensions::dispatch_context_with_system` / `has_context_with_system_handlers`；
  - `agent_loop::apply_context_with_system`：把 prux 的 `(system_prompt, messages)` 组装成完整
    transcript（`[0]` = system）交给扩展，取回**原样发送**的 `(messages, system_prompt)`——
    处理器可改写提示词（含工具声明所在的首位 system 消息）或删掉它（= 请求无系统提示词，
    对齐 pi 的"允许但提示"）；**只影响本次请求**，不改写会话历史（`agent.messages`/`system_prompt` 不动）；
  - 无扩展声明该 hook 时零开销（不组装完整 transcript）；
  - 与 pi 的差异：pi 的 system 消息是 transcript 里的真实条目（依赖 canonical context，
    见 13），prux 的提示词仍是独立状态，故边界处做一次双向映射。
  测试：`context_with_system_sees_and_rewrites_prompt`（断言改写后的提示词真的进入请求体）。

### 15. [0.87.0] Per-model image input limits（图片输入上限/无损缩放）
- **描述**：`models.json` 的 `inputLimits.images.resize`，作用于文件附件、图片 read 与工具结果图片（缓存安全缩放）。
- **prux 可用性**：此前无该配置（read 工具只有硬编码常量 2000x2000/4.5MB，且仅作用于 read）。
- **prux 状态**：✅ 已实现：
  - 新增 `src/utils/image.rs`：`ImageResizeLimits`（`maxWidth`/`maxHeight`/`maxBytes`/`jpegQuality`，
    缺失回退 pi 默认 2000x2000 / 4.5MiB base64 / q80）+ `prepare_image`（pi 策略：已合规无损
    直通 → 等比缩放 → PNG/JPEG 多档质量择优 → 0.75 逐步降尺寸）+ 尺寸/转换提示
    + `normalize_base64_attachment`（工具结果图片兜底）；
  - 目录接线：`ModelEntry.image_resize`（`find_model` 解析 `inputLimits`）→ `ModelConfig.image_resize`
    （唯一构造点 `model_config_from_entry`，模型切换自动同步）；
  - 应用点：read 工具（`ReadImageOptions.resize_limits`，替代硬编码常量）、
    CLI `@文件` 参数与 TUI `@图片` 引用（`file_processor`，遵 `images.autoResize` 设置）、
    工具结果附件（含扩展注入/替换的图片，超限才解码，避免重复编码）；
  - 顺带修正内联格式集合与 pi 对齐：gif/webp 不再转 PNG（bmp 等仍转 PNG）。
  - 未移植：`images.maxPerRequest` / `maxRequestBytes`（目录值极大，实际不触发）。
  - **补充（第九轮）**：请求级上限现已解析透传——`utils/image.rs` 新增 `ImageInputLimits`
    （`resize` + `maxPerMessage` + `maxPerRequest`）与 `InputLimits`（顶层 `maxRequestBytes`），
    `ModelEntry.input_limits` 承接目录 `inputLimits`，`modelOverrides.inputLimits` 按 pi
    `mergeInputLimits` 深合并（顶层/images 键覆盖、`images.resize` 字段级合并）；
    `ModelConfig.image_resize` 取自 `input_limits.images.resize`。
    与 pi 0.87.1 一致，这两个上限**只解析不强制**（pi `docs/models.md`："Pi does not yet rewrite
    or reject history based on them"），因此无运行时拒绝/丢弃行为。
  测试：`utils::image` 11 项（目录解析/无损直通/尺寸缩放/字节上限/格式转换/附件规范化）、
  `read_respects_per_model_resize_limits`、`model_entry_carries_catalog_image_limits`、
  `at_image_refs_apply_model_resize_limits`、`process_file_arguments_skip_unresizable_image`。

### 16. [0.86.0] 状态 spinner 移入编辑器边框
- **描述**：compaction / branch summarization / retry 的 spinner 由独立状态区移入默认编辑器
  边框，与 working indicator 并列；自定义编辑器需显式 opt-in。
- **prux 可用性**：prux 的 spinner 在独立状态区（`render/status.rs`）。
- **prux 状态**：❌ 确认不实现（纯观感调整；prux 的 working indicator 与 compaction/branch/retry
  spinner 分属状态区与编辑器边框两处，改动会牵动自定义编辑器 opt-in 协议，收益不足）。

### 17. [0.87.1] 新模型目录
- **描述**：
  - Anthropic：新增 `claude-opus-5-5`（adaptive thinking、1M 上下文）。
  - OpenAI / Codex / Copilot：新增 `gpt-6-sol`、`gpt-6-luna`。
  - xAI：默认模型改为 `grok-4.7`。
- **prux 可用性**：prux 已有 `claude-opus-5`（无 5-5）、`gpt-5.6-sol/luna`
  （无 `gpt-6-*`）、`grok-4.5/4.6`（默认 `grok-4.5`）。
- **建议**：高优先级（目录同步，成本低）。
- **prux 状态**：✅ 已实现——定点新增 `claude-opus-5-5`、`gpt-6-sol`、`gpt-6-luna`、`grok-4.7`，
  默认模型 `xai → grok-4.7`（同时把 `zai`/`zai-coding-cn` 默认对齐 pi 的 `glm-5.3`）。
  **后续**：用户已用全量 `sync-models.py` 完成同步（part2 + meta provider）；相应地 `deepseek-v4-flash → deepseek-flash`
  等改名已同步到全部测试/引用，套装测试恢复全绿。

---

## 三、移除的功能

#### [0.86.0] 从 OpenAI Codex 目录移除 GPT-5.4 / GPT-5.4 mini
- **移除原因**：对 ChatGPT 账号不可用（#9394）。
- **prux 影响**：prux 的 Codex 选择来自远程 pi.dev 目录（`model_refresh`），
  **不**来自 `assets/models/models.openai.json`；但此前 `openai-codex` 没有内嵌基线，
  离线/首启时 `--list-models openai-codex` 报「no model catalog」，且 `default_model_id`
  无 `openai-codex` 条目。
- **prux 状态**：✅ 已实现——新增内嵌基线 `assets/models/models.openai-codex.json`
  （由 pi 0.87.1 `openai-codex.json` 同步，8 个模型，无 `gpt-5.4*`），
  `SUPPORTED_PROVIDERS`/`baseline_models` 注册 `openai-codex`，默认模型
  `openai-codex → gpt-5.5`（对齐 pi `defaultModelPerProvider`）；补回归测试
  `codex_catalog_excludes_retired_gpt_5_4_models`。
  `openai` / `github-copilot` 的 `gpt-5.4` 按 pi 保留（默认模型仍是 `gpt-5.4`，与 pi 一致）。

---

## 四、破坏性变更（必须处理）

#### 1. [0.87.0] 移除 `shouldStopAfterTurn`，改用 `finishTurn`
- **变更**：`AgentOptions.shouldStopAfterTurn` / `AgentLoopConfig.shouldStopAfterTurn` 移除。
  新 `finishTurn` 在 assistant 与全部工具结果 finalize 后、`turn_end` 之前运行，
  返回 `{ action: "end" }` 结束（其决策在 `turn_end` 之后生效），返回 `undefined` 维持正常调度；
  error/aborted 响应仍为硬退出。
- **prux 影响**：`src/core/agent_loop.rs`、`src/core/agent_session.rs` 中存在
  `should_stop_after_turn`，语义为「仅正常响应后停止」。
- **prux 状态**：✅ 已实现——`should_stop_after_turn: FnMut(&TurnContext) -> bool` 改为
  `finish_turn: Option<Mutex<FinishTurn>>`（`FnMut(&TurnContext) -> Option<FinishTurnAction>`，
  `FinishTurnAction::{End, Continue}`）：
  - 调用时机：assistant 与全部工具结果 finalize 之后、`turn_end` **之前**；决策在 `turn_end`
    **之后**应用；
  - normal / error / aborted 三种响应都会调用，error/aborted 保持硬退出（决策忽略）；
  - `End` → 结束 run 且不 poll steering（队列保持原样）、不进下一轮 prepareNextTurn；
  - `Continue` → 无工具结果/steering 时经 `explicit_continuation` 保证一次额外 provider 请求
    （pi `explicitContinuation`），已有自然请求时不额外新增；
  - 子代理 max_turns 回调按 pi 迁移指引显式跳过 error/aborted，避免谓词副作用。
  测试：`finish_turn_end_terminates_before_next_turn`、`finish_turn_continue_forces_extra_request`、
  `finish_turn_runs_on_error_and_decision_ignored`。

#### 2. [0.87.0] `SessionEntry` 并集新增 `ContextEditEntry`
- **变更**：TypeScript 侧穷举 `SessionEntry` 的消费者必须处理 `context_edit`
  （`replacement: null` 表示省略）。
- **prux 影响**：`src/core/session_v4.rs` 的 entry 枚举/校验
  （`validate_entry_payload`）。新增条目类型需同步 schema、重放与归约逻辑。
- **prux 处置**：随「canonical context」功能一起做；未移植该功能前无需改动。

#### 3. [0.87.0] `SessionManager` 成为 `AgentSession` provider 上下文的权威来源
- **变更**：直接赋值 `session.agent.state.messages` 不再改变后续请求历史；
  需用 `SessionManager.inMemory(...)`、`session.navigateTree()` 或 `sessionManager` 追加 + `refreshContext()`。
- **prux 影响**：prux 的 worker 持有 `Agent` 并自行管理 messages；需确认上下文派生是否已以
  session 记录为唯一真源。
- **prux 状态**：✅ N-A（架构不同）——prux 没有「每次请求由 SessionManager 重新投影」的层，
  `Agent.messages` 本身就是 provider 上下文；但所有会改上下文的入口都经
  `Session::build_context_messages()`（session 记录 → messages）重建，二者由构造保证同步：
  `worker.rs:610/832`（会话切换/重建）、`agent_session.rs:1339`（树导航）、
  `agent_session.rs:1131`（压缩后重建）、`agent_session.rs:2741`（子代理物化）。
  因此 pi 的「赋值 messages 不再改变历史」在 prux 无对应语义变化（唯一的显式覆盖点是
  `prepare_next_turn` 返回的 `snap.context`，本就是设计好的接口）。

#### 4. [0.87.0] `TurnEndEvent` 扩展 + 新增 `AgentBeforeSettleEvent`
- **变更**：`TurnEndEvent` 增加必需边界字段；`ExtensionEvent` 并集新增
  `AgentBeforeSettleEvent`；`ExtensionRunner.emit()` 不再接受 `turn_end`，
  宿主用 `emitBoundary(baseEvent, buildContext)` 派发。
- **prux 影响**：`src/core/extensions/events.rs`、`src/core/extensions/hooks.rs`
  （`ExtensionHook::AgentEvent` / 事件派发）。
- **prux 状态**：✅ 已实现（prux 形态，不照搬 pi 的 entries/context-preview 机制）：
  - 新增 `ExtensionHook::Boundary` + `Extension::on_boundary(&Value) -> Option<BoundaryOutcome>`
    （`BoundaryOutcome { continue, end }`；多扩展按并集合并，`end` 优先）；
  - `turn_end` 边界由**内核循环**派发（`agent_loop::finalize_turn` / `finish_error_turn`），
    与 `finish_turn` 同一时机（turn_end 事件之前取决策、之后生效）；主代理与子代理一致；
  - `turn_end` 事件补边界字段：`turnIndex`（同时补到 `turn_start`）/ `messageEntryId` /
    `toolResultEntryIds` / `outcome`（`completed|aborted|error`）；error/aborted 轮也投递，
    但硬退出、决策忽略；
  - `agent_before_settle` 边界在 `emit_agent_settled` 之前派发（字段 `outcome` /
    `contextMessages` / `hasQueuedMessages` / `continuationIndex` / `agentScope`），
    `continue` 时本回合再发起一轮（pi `_runAgentPrompt` 同款循环：每次结算重新咨询，
    由扩展自限；abort 期间不续跑）；
  - `BoundaryOutcome` 增 `append`（pi `BoundaryResult.entries` 子集）：结算边界的 `continue`
    可携带要追加的 user 消息——内核先落库注入（发 message_start/end 事件）再起下一轮；
  - 边界事件带 `agentScope`（`main` | `subagent`）：pi 的结算边界只装在会话主 agent 上，
    prux 统一投递主/子代理，由扩展自行判定（goal 即只在 `main` 续跑）；
  - `finish_turn` 与边界合并：`end` 优先，否则任一 `continue` 即续跑（pi `explicitContinuation`）。
  - **内置消费者**（第六轮）：
    - loop-detect：命中（四个检测器任一）除 `AbortRun` 外还置位"回合后终止"，`turn_end`
      边界返回 `end`——本 run 在当前回合结束后确定性终止，不依赖 UI 事件循环处理 `AbortRun`，
      也不波及父 run（子代理命中只结束自己的 run）。`Boundary` 仅在 run 进行中（或已有未消费
      的终止标志）声明，避免让内核为每次 prompt 序列化整份 transcript 构造结算事件；
    - goal：`agent_before_settle`（主 agent / active / 无排队用户输入 / 非 aborted）返回
      `continue_with([续跑触发消息])`，取代原先经 UI `Continuation` 的自主续跑（该请求在 run
      进行中必被 UI 忙碌态丢弃）；预算耗尽的收尾轮改由 `turn_end` 边界 `continue` 起轮
      （子代理轮命中时留给主 agent）；空闲态起跑（`/goal <objective>`、`/goal resume`、
      Replace 确认）仍走 UI `Continuation`。
  测试：`turn_end_boundary_fields_and_continue`、`turn_end_boundary_reports_tool_result_entry_ids`、
  `turn_end_boundary_end_stops_run`、`turn_end_boundary_ignored_on_hard_exit`、
  `settle_boundary_continue_runs_another_round`、`settle_boundary_appends_messages_before_continuation`、
  `detection_ends_run_at_turn_end_boundary`、`settle_boundary_continues_when_goal_active`、
  `settle_boundary_yields_to_abort_queue_and_subagent`、`turn_end_accounts_and_budget_notifies`。

#### 5. [0.87.0] `agent_settled` 期间请求的 deferred run 延后到所有 settled 处理器结束后
- **变更**：处理器仍见 `ctx.isIdle() === true`，但同一通知派发内不再出现重入 `agent_start`。
- **prux 影响**：需检查 prux 是否有等价的 settled/idle 派发与重入防护（`agent_session.rs`）。
- **prux 状态**：✅ N-A（结构上已保证）——worker 的 sink 不内联改 UI，而是经
  `run_in_event_loop` 投到单一 UI 任务队列（`interactive.rs:108`），由主循环空闲间隙执行；
  `apply_sink_agent_settled`（`handlers/events.rs:514`）复位 busy 并 drain 队列时只
  `worker.prompt_batch(...)` 投命令，worker 在 busy 期间把命令排队，`agent_start` 作为
  独立队列项在本次派发结束后才到达 → 同一通知派发内不会重入。扩展自主续跑
  （`ExtensionUiRequest::Continuation`）还有显式 busy 门控（`interactive.rs:933`），
  忙碌时跳过以免抢跑用户输入。

#### 6. [0.86.0] Provider stream 输入由 `Context` 改为规范化 `TranscriptContext`
- **变更**：自定义 provider 需从 `context.messages` 用 `getCurrentSystemPrompt()` /
  `getCurrentTools()` 读取 system prompt 与工具声明。
- **prux 影响**：prux 的 provider 入参是 Rust 结构（`convert.rs`/各 provider），
  架构不同；此变更主要影响 pi 的 TS provider 接口。
- **prux 处置**：❌ 结构性 N/A（接口形态不同）；但需关注与之配套的「中途系统提示」语义
  （见新增功能 #3）。

#### 7. [0.86.0] `ToolCall.arguments` / `ToolResultMessage.details` 限制为 JSON 兼容值
- **prux 影响**：prux 使用 `serde_json::Value`，天然满足。
- **prux 处置**：❌ 结构性 N/A。

#### 8. [0.86.0] `user_bash` 改为 fail-closed
- **prux 影响**：prux 扩展体系为 Rust 内置，无 `user_bash` 处理器链。
- **prux 处置**：❌ 结构性 N/A。

---

## 五、移植优先级建议

### 高优先级（正确性 / 低成本高收益）——8 项已全部实现
1. ✅ **GIF 魔数误判**——`utils/mime.rs` 仅认 `GIF87a`/`GIF89a`（0.87.0 #9755）。
2. ✅ **纯图片 user 消息空 text part**——`completions.rs::convert_user_message` 过滤空 text（0.87.1 #9797）。
3. ✅ **Anthropic OAuth 版本**——`anthropic.rs` `CLAUDE_CODE_VERSION = 2.1.280`。
4. ✅ **prompt 模板 frontmatter 告警**——`prompt_templates.rs` 校验 + 启动/`/reload` Warning。
5. ✅ **默认开启 strict-prefer 工具采样**——移除 `PRUX_EXPERIMENTAL` 门控（0.86.0）。
6. ✅ **工具耗时 ≥1min 用分:秒**——`render/messages.rs::format_tool_duration`。
7. ✅ **Per-model compaction budgets**——`compaction.modelOverrides` + `for_model`。
8. ✅ **新模型目录**——`claude-opus-5-5`、`gpt-6-sol`、`gpt-6-luna`、`grok-4.7`（默认）。

> 验证：`cargo test`（1536 单测 + 全部集成测试）与 `cargo clippy --all-targets` 均通过。

### 中优先级审计结果（第二轮，对照 pi 0.87.1 源码逐项审计）

> 分类：【已修复/已实现】prux 已改动并补测试；【已对齐】行为与 pi 0.87.1 等价，不改；
> 【N-A】结构性不适用；【待议】改动面大/需进一步判断；【不做】功能/性能改进类。

| 项（pi 修复号） | 分类 | 结论 / prux 位置 |
|---|---|---|
| z.ai `Prompt too long` overflow (#9805) | 已对齐 | `is_overflow_error_str` 已含 `prompt too long` |
| 无 body 400/413 误判 overflow (#9482) | 已实现 | 仅 Cerebras（provider 门控）计为溢出：`is_bodyless_status_error` |
| overflow 静默（z.ai）/ length-stop（Xiaomi） | 已实现 | `message_is_context_overflow` 三态（error/静默/length-stop） |
| 运行中阈值压缩跳过超大尾随工具结果 (#9740) | 已修复 | `find_compaction_cut_index`：tool result 计入保留预算 |
| 取消期间压缩竞争 (#9340/#9777) | N-A | worker 独占 Agent + drop-future 取消；`is_compacting` 仅信息位 |
| split-turn 摘要 prompt (#9908) | N-A | prux 压缩不拆 turn（保留整轮），无 prefix prompt |
| 被放弃 attempt 残留（0.87.0） | N-A（冻结） | 依赖 canonical context / `ContextEditEntry`（会话格式冻结） |
| 精确 session ID 只读 header (#9601) | 已修复 | `session_picker::find_session_by_id` 仅读首行 |
| session tree 导航 vs compaction (#9179) | N-A | worker 串行处理命令 |
| Copilot GPT 走错 adapter (#9253) | 已修复 | `models.github-copilot.json`：`gpt-6-astra` api → `openai-responses` |
| OpenAI Codex Off effort (#9191) | 已修复 | `codex.rs` 按 pi 映射（off→省略/none/映射值） |
| Cloudflare 520 重试 (#9627) | 已修复 | agent 级 RETRYABLE +`"520"` |
| Azure 峰值容量重试 (#9669) | 已修复 | +`"currently experiencing high demand"` |
| agent 级退避上限 (#8826) | 已修复 | `MAX_AGENT_RETRY_DELAY_MS=60s` 封顶 |
| Fireworks/Vercel unsigned thinking replay (#9323/#9676) | 已实现 | `anthropic.rs` `compat_allow_empty_signature`：保留 thinking 块 + 空 signature |
| Anthropic relay signed thinking (#9188) | 已对齐 | 重放按请求 model；无 fallback 定价特性 |
| Google/Vertex thinking level (#9455) | 已实现 | `google.rs` 按目录支持级别钳制（`uses_google_thinking_level` + `disabled_thinking_config`） |
| Responses 错误 provider 标注 (#9298) | 已对齐 | 无 `OpenAI API error` 前缀，本无此缺陷 |
| Cerebras strict schemas (#9804) | 已对齐 | `supports_strict_mode` 默认 false，已排除 |
| 未知端点 strict schema (#9816) | 已对齐 | 默认非 strict（prux 比 pi 更保守） |
| EventStream drain O(n²) (#9055) | N-A | 无事件队列（同步回调），SSE buffer 有界 |
| `TransformContext` 与 pi `context` 边界 (#9789/#9822) | N-A | system prompt 独立传入，`agent.messages` 无 system 消息 |
| 扩展工具缺参数 schema 注册期拒绝 (#9300) | N-A | 扩展为 Rust 内置，无动态注册路径 |
| 登录后过早报 missing model（Radius） | N-A | prux 无 Radius；内嵌目录登录后即可用 |
| skill 补全按裸名排序 (#9120) | 已修复 | `suggest.rs` 去 `skill:` 前缀参与模糊匹配 |
| CJK 文件补全边界与路径引号 (#9746) | 已实现 | 分隔符含 CJK 标点；含分隔符的路径加引号 |
| LaTeX legacy 字体切换回落原文 (#8827) | 已修复 | `latex.rs` 新增 `FONT_SWITCH_COMMANDS` |
| LaTeX `cases` 居中 / 嵌套上下标 (#9564/#7929) | 已实现 | 新增 `LayoutNode::Script` 垂直堆叠；`cases` 居中矩阵节点 |
| 剪贴板失败上报 + OSC52 门控 (#9618/#9688) | 已实现 | 仅远程/无显示 Linux 用 OSC52；失败返回平台指引 |
| `--resume`/`--continue` 渐进与加速 | 已实现 | 见二.11（CLI 选择器渐进加载、TUI `/resume` 分批、`--continue` header 提前停止） |
| transcript-aware updates / canonical context / `context_with_system` / per-model image limits | 已实现 / 不实现 | `context_with_system`（二.14）、per-model image limits（二.15）、canonical context（二.13，第十一轮）均已实现；transcript-aware（二.3）**不实现（用户决定）** |
| Prompt cache warming / Radius provider 等 | 已完成 / 不实现 | 二.1 缓存保温第九轮实现、第十一轮标记**已完成**；Radius provider（二.4）**不实现（用户决定）**。Meta provider（二.12）API key 路径已实现、Muse 订阅 OAuth **不实现**；`compat.allowedFallbackModels`（二.8）与 `/bug` 本地导出（二.2）已实现（失败回合提示行 **不实现**） |

**本轮中优先级验证**：`cargo test`（1548 单测 + 全部集成测试）与
`cargo clippy --all-targets` / `cargo fmt --check` 均通过。

### 剩余未实现项（第十一轮定案）

> **口径（第十一轮）**：剩余未实现项 **7 项**，**全部为「已明确不实现」**（用户决定 / 上游已移除），
> 无待议项。二.13（canonical session context / `ContextEditEntry`）与一.2（被放弃 attempt 残留）
> 已在第十一轮实现；二.1（prompt cache warming）标记为**已完成**（与 pi 的两处差异判定为可接受，
> 见二.1）。已实现项的详情见各自小节，本节不重复。

**已明确不实现（无需再评估）**

1. **二.3 transcript-aware prompt/tool updates（会话内系统提示与工具变更）** —— ❌ **不实现（用户决定）**
   - 功能：中途修改 system prompt / 工具集后写进 transcript，resume 与分支导航后仍生效，
     并在支持缓存的模型上保留缓存前缀；模型能力由 compat
     `supportsMidConvoSystemMessages` / `supportsMidConvoToolChanges` 判定。
   - prux 现状：`core/system_prompt.rs` 每请求按当前状态组装，无 per-entry 快照；
     目录中 76 处 `supportsMidConvoSystemMessages` / 5 处 `supportsMidConvoToolChanges` 未被使用。
   - 不实现理由：与二.13 canonical context 绑定；pi 侧该功能以 transcript 内 system 消息为载体，
     prux 落地需会话格式演进（`AgentMessage` 携带 sections / toolsAdded / toolsRemoved 或等价的
     投影层）+ 三个 transport 的原生序列化/折叠。工具集变更 prux 已以
     `ActiveToolsChange` entry 持久化并在 resume 时还原，已满足「恢复后仍生效」的基本诉求。

2. **二.7 `ctx.modelRegistry.stream()` / `streamSimple()`** —— ❌ **不实现（用户决定）**
   - 功能：扩展用**已配置 provider + 已解析鉴权**发起模型调用。
   - prux 现状：扩展为 Rust 内置，无等价公开 API；但已有 OpenAI 兼容 proxy 扩展
     （`src/extensions/proxy/`）可让外部客户端走 prux 的 provider，能力部分重叠。
   - 不实现理由：prux 扩展是编译进二进制的 Rust trait，无 pi 那种可下发任意 JS 的公开宿主 API；
     向外部扩展开放「用已解析凭据代发模型请求」会引入凭据外泄面。

3. **二.4 离线 Radius 目录** —— ❌ **不实现（用户决定）**
   - 功能：内嵌 Radius（`pi-messages` 网关）目录，离线/即时选模型，叠加缓存 + 实时网关目录。
   - prux 现状：无 Radius provider（`login_registry.rs:11` 明确「暂不实现」），需先做
     `pi-messages` API + OAuth，成本高。
   - 不实现理由：前置依赖是网关 provider 本体（API + OAuth + 计费/用量上报），内嵌目录只是其附属；
     且该特性的收益（首启/离线即可选模型）对 prux **不成立**——prux 目录本就内嵌
     （`assets/models/*.json`），离线已可选模型。

4. **二.10 Fireworks / OpenAI 原生 deferred tool loading** —— ❌ **不实现（上游已移除）**
   - 功能：以 `ToolSearch` / `tool_search` 作为 loader 名做 prompt 前缀延迟加载
     （pi #9323；OpenAI Responses 侧另有 `compat.deferredToolsMode` / `supportsToolSearch`）。
   - prux 现状：无 deferred tools；目录 17 处 `supportsToolSearch`（`openai`/`openai-codex`）未使用。
   - **⚠️ 复核结论（第九轮，对照 pi 0.87.1 源码）**：该特性在 pi 0.86.0
     （commit `9e05370b2` "Mid conversation system messages"）**已被删除并取代**——`compat.deferredToolsMode` /
     `supportsToolReferences` / `ToolResultMessage.addedToolNames` / `utils/deferred-tools.ts` 均不存在，
     `ToolSearch` / `tool_search` 只在 CHANGELOG 里出现过，代码中已无引用。
     现行形态是**二.3 的工具变更半边**：工具增删以 `SystemMessage.toolsAdded/toolsRemoved` 写进 transcript，
     再由各 transport 原生序列化——Anthropic Messages（`supportsMidConvoToolChanges`，
     `__pi_deferred_placeholder__` + `defer_loading: true` + `tool_addition`/`tool_removal` 块 +
     `mid-conversation-tool-changes-2026-07-01` beta）、OpenAI Responses（`additional_tools`，
     或 `supportsToolSearch` 时的合成 `tool_search_call`/`tool_search_output`）、
     OpenAI Completions（Kimi：带 `tools` 的 system 消息）。
     因此二.10 **不能独立移植**，需与二.3（canonical context / transcript system messages）一起做。
   - **决定（用户拍板，第九轮）**：二.3 与二.10 都**不做**（需会话格式演进 + 三个 transport 的原生序列化）。
     同时清理 prux 中该特性的**残留死管道**：`AgentMessage.added_tool_names`
     （pi `ToolResultMessage.addedToolNames`）、`ToolResult.added_tool_names`、
     `FinalizedToolCall.added_tool_names`、工具结果 JSON 的 `addedToolNames` 字段——
     均无任何读取方（不触发工具注入），已全部移除；
     目录 `supportsToolSearch` / `supportsAdditionalTools` 等 compat 元数据保留
     （pi 0.87.1 现行字段，且 prux 与 pi 一样只作为目录数据）。

5. **二.16 状态 spinner 移入编辑器边框** —— ❌ **不实现（已确认）**
   - 纯观感调整；prux 的 working indicator 与 compaction/branch/retry spinner 分属状态区与
     编辑器边框两处，改动会牵动自定义编辑器 opt-in 协议，收益不足。

6. **二.12 Muse 订阅 OAuth 自动刷新** —— ❌ **不实现（用户决定）**
   - API key 路径**已实现**（目录 `models.meta.json` + `META_API_KEY` + 默认模型 `muse-spark-1.3`）；
     未做的只有 pi 的 `isSubscription: true` + `loadMetaOAuth` 自动轮换 Model API key。
   - 不实现理由：需实现 Meta 订阅 OAuth 流程并管理**自动轮换的凭据类型**（prux `oauth.rs` 的
     既有 provider 均为固定 client 的授权码/设备码流程），而 API key 路径已能完整使用 Muse Spark。

7. **二.2 失败回合后的「/bug 提示」行** —— ❌ **不实现（用户决定）**
   - pi `maybeSuggestBugReport`；prux 已有启动期 crash 提醒（`core/crash_log.rs`），
     而「上一轮出错就提示报 bug」在 prux 价值有限（错误信息已在 transcript 可见）且会重复打扰。


**已实现详情**：二.1 / 二.2 / 二.6 / 二.8 / 二.11 / 二.13 / 二.14 / 二.15 / 一.2 / 四.4 的完整状态
见各自小节（第十一轮：二.13 与 一.2 已实现、二.1 已完成）。

### 结构性 N/A（无需改动，仅记录）
- `ToolCall.arguments` JSON 限制、`user_bash` fail-closed、provider `TranscriptContext`
  接口形态、模糊搜索 JS 性能、全屏 footer 空行。
