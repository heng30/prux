# Prux 迁移指南：pi 0.84.0 → 0.85.1

> 本文档仅列出当前 prux 项目已实现的功能在 pi 0.84.0→0.85.1 期间的相关变更。
> 不涉及 prux 未实现的功能，也不涉及 0.84 之前版本的变更。
>
> **更新记录**：高优先级 10 项已逐项审计（对照 pi 0.85.1 源码）并完成修复，状态标记见各条 `prux 状态` 与第四节勾选。

---

## 一、Bug 修复（需要评估是否移植）

### 1. 工具系统 (tools)

#### [0.84.3] edit 工具：单对象输入校验
- **问题**：`edit` 工具接收单个编辑对象（非数组）时校验失败。
- **修复**：在 coding-agent 和 harness 两个 edit 工具中均接受单对象作为单元素数组。
- **prux 影响**：需要检查 `core/tools/edit.rs` 的输入解析逻辑是否同样需要容错。
- **prux 状态**：✅ 已对齐——prux `normalize_edit_args` 已覆盖 JSON 字符串/单对象/legacy 顶层键三种输入形态，无需修改。

#### [0.84.4] compaction 和 branch summary 不应暴露工具
- **问题**：compaction 和 branch summarization 请求向 provider 发送了 tools 定义，导致 `toolChoice: "none"` 被强制设置。
- **修复**：compaction/summary 请求不再暴露 tools。
- **prux 影响**：检查 `core/compaction/` 模块的 prompt 构建，确保不传递 tools 给 summarization 调用。

#### [0.84.4] 大工具结果跨自动压缩阈值时的处理
- **问题**：当工具结果使上下文超过自动压缩阈值时，结果会先发送给 provider 再压缩，导致问题。
- **修复**：在同一 run 中，工具执行后、下一个 assistant 响应前进行压缩，并在 run 恢复时恢复交互进度。
- **prux 影响**：检查 `core/agent_session.rs` 中自动压缩触发时机。

#### [0.85.0] bash/edit/find/grep/ls/read/write 忽略 ctx.cwd
- **问题**：所有内置工具忽略了扩展设置的 `ctx.cwd`。
- **修复**：工具现在正确使用 `ctx.cwd`。
- **prux 影响**：检查所有工具是否正确使用了上下文中的工作目录。
- **prux 状态**：✅ 已对齐——prux 工具执行统一读取运行时 `self.cwd`（ToolDef 静态注册、无闭包捕获 cwd），架构上不存在 pi 的「创建时闭包 cwd」问题，无需修改。

#### [0.85.0] write 工具报告 UTF-16 代码单元数作为字节数
- **问题**：write 工具在返回时报告了误导性的字符计数。
- **修复**：移除了误导性的计数。
- **prux 影响**：检查 `core/tools/write.rs` 的返回值。
- **prux 状态**：✅ 已修复（e1d7bae）——移除误导性计数，输出改为 `Successfully wrote to <path>`。

#### [0.84.0] find 工具：POSIX/Windows 根路径问题
- **问题**：从文件系统根目录执行 find 时，结果丢失第一个路径分段或获得重复的尾部分隔符。
- **修复**：修正路径处理。
- **prux 影响**：检查 `core/tools/find.rs` 的路径处理。

#### [0.84.3] 单对象 edit 工具输入（与上面 0.84.3 重复项合并）

### 2. 会话管理 (session)

#### [0.84.4] 恢复的会话损坏下一条追加条目
- **问题**：恢复的会话在 JSONL 文件缺少尾部换行符时，会损坏下一条追加的条目。
- **修复**：正确处理缺少尾部换行的文件。
- **prux 影响**：检查 `core/session_manager.rs` 的追加逻辑。

#### [0.85.0] 并发会话共享互相覆盖
- **问题**：多个并发会话共享时会互相覆盖。
- **修复**：修复并发写入。
- **prux 影响**：检查 session_manager 的并发安全性。

#### [0.85.0] 导入会话覆盖同名现有会话
- **问题**：导入的会话会覆盖同名的现有会话。
- **修复**：避免覆盖。
- **prux 影响**：检查会话导入逻辑。

#### [0.85.0] session fork 丢失 compaction boundary
- **问题**：fork 的会话丢失了 compaction boundary。
- **修复**：fork 时保留 compaction boundary。
- **prux 影响**：检查 session fork 实现。
- **prux 状态**：✅ 已对齐——最初用「Compaction entry 内联 `retainedTail`」规避 pi 的 `firstKeptEntryId` 悬空指针问题（49d0937 回归测试）；**后续已改为与 pi 同构**：Compaction 落盘 `firstKeptEntryId`（保留区间的原始条目 id），投影按区间把原始条目带进上下文——内联副本会让「保留条目」不再是真实条目（压缩后追加的 `context_edit` 命中不了、resume 后编辑失效），详见 `session_manager::build_context_projection`。**不保留旧格式兼容**：`retainedTail` 字段已从 `Entry::Compaction` 移除，旧会话文件里的内联尾部不再被读取（其保留区间无法还原成条目 id），投影退化为「摘要 + 压缩之后的条目」。

#### [0.85.0] 在活跃轮次结束前的内存 session fork
- **问题**：在活跃轮次结束前创建的内存 session fork 有问题。
- **修复**：修复 fork 时机。
- **prux 影响**：检查 session fork 的时机。

#### [0.84.0] JsonlSessionRepo 全局会话 ID 冲突
- **问题**：JsonlSessionRepo 在不同工作目录间强制全局会话 ID 唯一。
- **修复**：ID 现在在每个工作目录内唯一。
- **prux 影响**：检查 session ID 的唯一性范围。

#### [0.84.0] JSONL session forks 和 torn-tail 修复的原子性
- **问题**：fork 和尾部修复不是原子写入，中断后可能导致损坏。
- **修复**：使用原子写入。
- **prux 影响**：检查 session 写入的原子性。

### 3. 压缩 (compaction)

#### [0.84.4] compaction 和 branch summary 暴露工具（同上 tools 部分）
#### [0.84.4] 跨阈值大工具结果（同上 tools 部分）

#### [0.85.0] branch summaries 在 reasoning 消耗输出上限时失败
- **问题**：当 reasoning 消耗了之前的 2048 token 输出上限时，branch summary 生成失败。
- **修复**：正确处理 reasoning 输出上限。
- **prux 影响**：检查 branch summarization 的输出上限处理。

#### [0.85.0] abort 未取消进行中的手动压缩
- **问题**：abort 报告成功但未实际取消进行中的手动压缩。
- **修复**：abort 现在正确取消压缩。
- **prux 影响**：检查 abort 信号在 compaction 中的传播。

#### [0.84.3] 截断的 compaction/branch summary 被持久化
- **问题**：当生成达到输出 token 上限时，截断的 compaction 和 branch summary 会被持久化。
- **修复**：不持久化截断的结果。
- **prux 影响**：检查 compaction 结果的完整性验证。
- **prux 状态**：✅ 已修复（dd8e4d3）——`simple_completion` 检测 `stop_reason=="length"` 并返回错误，compaction 主摘要与 branch summary 两条路径均不再持久化截断结果。

#### [0.84.3] 阈值自动压缩在 provider 省略 streaming usage 时被跳过
- **问题**：当 provider 省略 streaming usage 数据时，阈值自动压缩被跳过。
- **修复**：即使没有 usage 数据也触发阈值压缩。
- **prux 影响**：检查自动压缩的触发条件。

#### [0.84.0] manual compaction 与阈值自动压缩竞争
- **问题**：手动压缩和阈值自动压缩会互相竞争。
- **修复**：避免竞争条件。
- **prux 影响**：检查压缩锁机制。

#### [0.84.0] 截断响应未触发重试
- **问题**：低于预期输出限制的截断响应会结束运行，而不是压缩并重试一次。
- **修复**：截断响应会触发压缩和重试。
- **prux 影响**：检查截断响应的重试逻辑。

### 4. Provider / 流式处理

#### [0.85.0] provider stream 发出不兼容的事件序列和自定义 tool-call delta
- **问题**：继承的 provider stream 发出不兼容的事件序列。
- **修复**：修复事件序列。
- **prux 影响**：检查流式解析逻辑。

#### [0.85.0] 代理 HTTP 请求在工具调用后挂起
- **问题**：通过代理的纯 HTTP provider 请求在工具调用后挂起。
- **修复**：使用 CONNECT 隧道。
- **prux 影响**：检查 HTTP 代理逻辑（`core/provider/` 中的 reqwest 配置）。

#### [0.85.0] NO_PROXY 匹配根域名和子域名
- **问题**：`NO_PROXY` 环境变量的根域名和子域名匹配不正确。
- **修复**：修复匹配逻辑。
- **prux 影响**：检查代理/NO_PROXY 处理。

#### [0.85.0] OpenAI Codex SSE 解析
- **问题**：OpenAI Codex SSE 解析无法处理不跟空行的终端事件。
- **修复**：修复解析。
- **prux 影响**：检查 `core/provider/codex.rs` 的 SSE 解析。
- **prux 状态**：✅ 已修复（dd8e4d3）——`SseAccumulator::finish()` 在流结束时 flush 未终止残帧，无尾随空行的终端事件不再丢失。

#### [0.85.0] GitHub Copilot reasoning level 未发送
- **问题**：GitHub Copilot Claude Fable 5 请求未发送选定的 reasoning level。
- **修复**：正确发送 reasoning level。
- **prux 影响**：检查 Copilot provider 的 reasoning level 传递。

#### [0.84.3] OpenAI-compatible reasoning replay
- **问题**：reasoning replay 未保持和重发 assistant-level `reasoning_details`。
- **修复**：保持原始顺序和内容。
- **prux 影响**：检查 reasoning replay 逻辑。

#### [0.84.3] Anthropic server-side refusal fallback pricing
- **问题**：Anthropic server-side fallback 响应使用了请求模型而非返回的 fallback 模型的定价。
- **修复**：使用正确的模型定价。
- **prux 影响**：检查 usage 定价计算。

#### [0.84.3] Amazon Bedrock 推理重放
- **问题**：Amazon Bedrock 丢弃和重放非 Anthropic 模型的 opaque redacted reasoning 失败。
- **修复**：正确处理。
- **prux 影响**：如果支持 Bedrock，需检查。

#### [0.84.2] OpenAI Responses function tool 丢失 namespace
- **问题**：OpenAI Responses function 和 custom tool calls 在 streaming/proxying/replay 时丢失 namespace。
- **修复**：保持 namespace。
- **prux 影响**：检查 responses API 的 tool call namespace 处理。

#### [0.84.2] Google Generative AI tool call 停止类型误判
- **问题**：Google Generative AI 和 Vertex AI 带 tool call 的响应将 output-limit 或 provider-error 停止误判为正常 tool use。
- **修复**：正确识别停止类型。
- **prux 影响**：检查 Google provider 的流式解析。

#### [0.84.1] Anthropic stream 丢弃初始 content-block 中的文本
- **问题**：Anthropic stream 丢弃了初始 content-block 事件中的文本或 thinking。
- **修复**：保留初始内容。
- **prux 影响**：检查 Anthropic provider 流式解析。
- **prux 状态**：✅ 已修复（dd8e4d3）——`content_block_start` text 分支保留初始文本并以 TextDelta 发出（thinking 分支原已保留初始 thinking/signature）。

#### [0.84.0] Google history conversion 丢弃 signed empty text 和 thinking blocks
- **问题**：Google history conversion 丢弃了 replay 所需的 signed empty text 和 thinking blocks。
- **修复**：保留这些 blocks。
- **prux 影响**：检查 Google provider 的 history 转换。
- **prux 状态**：✅ 已修复（dd8e4d3）——text 分支保留 signed empty blocks 并回传 `thoughtSignature`（thinking 分支原已对齐）。

### 5. 系统提示 / 技能

#### [0.84.3] skills 在 Bash 为唯一启用工具时不可用
- **问题**：当 Bash 是唯一启用的工具时，skills 不可用。
- **修复**：修复 skill 发现。
- **prux 影响**：检查 `core/skills/` 的发现逻辑。

#### [0.84.2] 根目录 Markdown 文件被报告为 broken skills
- **问题**：skills 目录中的 `README.md` 等根文件被报告为 broken skills。
- **修复**：只有声明有效 frontmatter 的文件才被视为 skill。
- **prux 影响**：检查 skill frontmatter 解析。

#### [0.84.3] 嵌套 Markdown skills 在 `.agents/skills/` 分组目录中未被发现
- **问题**：嵌套在分组目录中的 Markdown skills 未被发现。
- **修复**：递归发现。
- **prux 影响**：检查 skill 递归发现。

#### [0.84.3] 自定义系统提示拼接 CWD
- **问题**：自定义系统提示将当前工作目录与后续追加的提示内容拼接。
- **修复**：正确分隔。
- **prux 影响**：检查 `core/system_prompt.rs` 的构建逻辑。

#### [0.84.3] UTF-8 BOM 阻止 frontmatter 加载
- **问题**：UTF-8 BOM 标记阻止 frontmatter 和用户配置文件加载。
- **修复**：忽略 BOM。
- **prux 影响**：检查文件读取是否处理了 BOM。
- **prux 状态**：✅ 已修复（c097dce）——新增 `utils::strip_bom`，应用于 skills/prompt_templates frontmatter 与 settings.json/auth.json 读取。

### 6. TUI / 渲染

#### [0.85.0] 管理的 fd 和 ripgrep 在 Linux musl 系统上下载失败
- **问题**：Linux musl 系统上工具下载失败。
- **修复**：修复下载逻辑。
- **prux 影响**：检查外部工具安装逻辑。

#### [0.85.0] branch summaries 在 reasoning 消耗输出上限时失败
- **问题**：同上 compaction 部分。

#### [0.84.4] thinking 可见性切换清除正在运行的 Bash 工具的部分输出
- **问题**：切换 thinking 可见性时清除正在运行的 Bash 工具的部分输出。
- **修复**：保留输出。
- **prux 影响**：检查 TUI 中 thinking 切换与工具输出的交互。

#### [0.84.3] invalid settings 文件在 TUI 中不可见
- **问题**：无效的 settings 文件在交互启动时容易被忽略。
- **修复**：在 TUI 中显示警告。
- **prux 影响**：检查 settings 加载时的错误提示。
- **prux 状态**：✅ 已修复（c097dce）——新增 `settings_load_error()`，交互启动时对无效 settings 文件以 Warning 提示而非静默忽略。

### 7. 扩展系统 (extensions)

#### [0.84.4] extension messages with triggerTurn: false 插入位置错误
- **问题**：`triggerTurn: false` 的扩展消息在 agent 运行时插入到 tool call 和 result 之间，导致 provider 拒绝。
- **修复**：延迟到当前轮次的 tool result 完成后再追加。
- **prux 影响**：检查扩展消息的注入时机。

#### [0.84.3] 扩展工厂失败后遗留状态
- **问题**：扩展工厂失败后，事件订阅、provider 注册和默认标志状态仍然活跃。
- **修复**：清理失败扩展的状态。
- **prux 影响**：检查扩展加载失败的清理逻辑。

#### [0.84.3] 扩展 TUI 方法包装器无限递归
- **问题**：扩展 TUI 方法包装器在委托给原始方法时无限递归。
- **修复**：修复递归。
- **prux 影响**：检查扩展 TUI hook 的实现。

#### [0.84.0] 扩展事件监听器在 session 重载后残留
- **问题**：扩展事件总线监听器在 session 重载和 dispose 后仍然存活。
- **修复**：正确清理。
- **prux 影响**：检查事件系统的生命周期管理。

### 8. OAuth / 认证

#### [0.84.0] OAuth token refresh 死锁
- **问题**：OAuth token refresh 在请求卡住时不释放 credential-store 锁。
- **修复**：释放锁。
- **prux 影响**：检查 OAuth token refresh 的锁机制。

#### [0.84.3] auth.json 和 models-store.json 写入覆盖文件权限
- **问题**：写入 auth.json 和 models-store.json 时覆盖了管理员管理的文件权限和 ACL。
- **修复**：保留文件权限。
- **prux 影响**：检查文件写入是否保留权限。

#### [0.84.0] 长期运行会话使用过时凭证
- **问题**：另一个进程更新 auth.json 后，长期运行的会话使用过时凭证。
- **修复**：序列化并发凭证读取并延迟启动。
- **prux 影响**：检查凭证缓存刷新机制。

### 9. 其他

#### [0.84.3] dash 前缀提示被解析为选项
- **问题**：dash 前缀的提示被解析为命令行选项。
- **修复**：支持 `--` 作为选项终止分隔符。
- **prux 影响**：检查 CLI 参数解析（`cli/args.rs`）。

#### [0.84.0] 手动 compact 期间排队的消息失败
- **问题**：在手动 `/compact` 期间排队的消息会失败。
- **修复**：压缩完成后发送排队消息。
- **prux 影响**：检查 compaction 期间的消息队列处理。
- **prux 状态**：⛔ 不适用——prux worker 独占 Agent + 命令队列天然排队，compact 期间消息不失败；无 pi 的「compaction 中 prompt 抛错」守卫及对应 bug。

---

## 二、新增功能（需要评估是否移植）

### 1. [0.85.0] Persistent Claude thinking effort
- **描述**：支持 Anthropic 传输保留每轮的 thinking effort，并从签名 thinking 不匹配中安全恢复。
- **prux 可用性**：已实现 Anthropic provider。此功能可增强 thinking level 的持久性。
- **建议**：高优先级，提升 thinking level 的可靠性。

### 2. [0.85.0] Restorable in-memory sessions
- **描述**：通过 SDK 恢复外部存储的会话条目。
- **prux 可用性**：SDK 功能，对 prux 核心影响较小。
- **建议**：低优先级。

### 4. [0.84.4] Terminal capability overrides
- **描述**：覆盖检测到的终端超链接、图像和 truecolor 支持。
- **prux 可用性**：prux 已有 terminal_caps 模块。
- **建议**：中优先级，允许用户覆盖终端能力检测。

### 5. [0.84.4] Extension UI prompt events
- **描述**：扩展可以区分活跃 agent 工作和等待 `ctx.ui` 提示的时间。
- **prux 可用性**：扩展系统已实现，可添加这些事件。
- **建议**：中优先级，改善扩展的 UI 交互感知。

### 7. [0.84.3] PowerShell tool
- **描述**：Windows 上可选的原生 PowerShell 命令执行工具。
- **prux 可用性**：已实现 powershell tool。
- **建议**：已实现，检查是否与最新变更对齐。

### 9. [0.84.3] Model and thinking controls (/thinking selector)
- **描述**：使用 `/thinking` 选择 thinking level，搜索默认选项，保持选择会话范围，Ctrl+S 显式持久化。
- **prux 可用性**：已有 thinking level 管理。
- **建议**：中优先级，增强 thinking level 的 UI 控制。

### 10. [0.84.2] Configurable default tools
- **描述**：全局或按项目选择启动时的内置工具。
- **prux 可用性**：已有 ToolSelection 策略。
- **建议**：中优先级，允许配置默认工具集。

### 12. [0.84.1] Qwen Token Plan Individual
- **描述**：内置 provider 支持 Qwen Token Plan Individual 订阅模型。
- **prux 可用性**：已有 model_resolver 和 provider 系统。
- **建议**：低优先级，模型目录更新。

### 14. [0.84.1] Authentication readiness checks (pi auth check)
- **描述**：使用 `pi auth check` 验证 provider/model 凭证。
- **prux 可用性**：已有 auth_command。
- **建议**：中优先级，增强凭证验证能力。

### 15. [0.84.1] Terminating blocked tool calls
- **描述**：扩展 `tool_call` 处理器可以在没有额外模型调用的情况下停止全终止批次。
- **prux 可用性**：扩展系统已实现 tool call 事件。
- **建议**：中优先级，增强扩展的工具调用控制。

### 16. [0.84.2] Constrained tool sampling (experimental)
- **描述**：实验性 JSON-schema 约束采样，用于 read、bash、edit、write 工具。
- **prux 可用性**：需要 provider 端支持。
- **建议**：低优先级，实验性功能。

---

## 三、破坏性变更（必须处理）

### 1. [0.84.0] message_update 事件格式变更
- **变更**：`message_update` 事件现在仅发出 `assistantMessageEvent` delta，移除了累积的 `message` 和 `assistantMessageEvent.partial` 字段。
- **影响**：如果 prux 的事件消费者依赖这些字段，需要更新。
- **建议**：检查 `core/agent_session.rs` 中的事件发射逻辑。

### 2. [0.84.0] ModelRegistry API 变更
- **变更**：`getApiKeyAndHeaders()` 返回 `ProviderHeaders`，值为 `string | null`；`refresh()` 接受 `ModelsRefreshOptions` 并返回 `ModelsRefreshResult`。
- **影响**：如果 prux 实现了自定义 provider 的 refresh 逻辑，需要适配。
- **建议**：检查 model_refresh.rs 和 provider 注册逻辑。

### 3. [0.84.0] Session API 重写
- **变更**：v4 lane-based `Session`, `SessionStorage`, `SessionRepo` API 替代旧 API；移除 legacy JSONL 和 in-memory repository API。
- **影响**：prux 已移植 v4 session，但需确认是否完全对齐。
- **建议**：审查 session_manager.rs 的 API 对齐情况。

### 4. [0.84.3] GoogleThinkingLevel 类型重命名
- **变更**：`GoogleThinkingLevel` 重命名为 `GoogleApiThinkingLevel`，新增 `ResolvedGoogleThinkingLevel`。
- **影响**：如果 prux 中有对应的类型定义，需重命名。
- **建议**：检查 Google provider 中的 thinking level 类型。

---

## 四、移植优先级建议

### 高优先级（Bug 修复，影响正确性）
1. ✅ 已对齐　edit 工具单对象输入校验（prux `normalize_edit_args` 已覆盖）
2. ✅ 已对齐　所有工具支持 ctx.cwd（工具统一使用运行时 cwd）
3. ✅ 已对齐　session fork 丢失 compaction boundary（初版用内联 retainedTail 规避悬空指针，后改为 pi 同构的 `firstKeptEntryId`；补回归测试 49d0937 + fork/tree 回归）
4. ✅ 已修复　截断的 compaction/branch summary 不应被持久化（dd8e4d3）
5. ✅ 已修复　OpenAI Codex SSE 解析（EOF 残帧 flush，dd8e4d3）
6. ✅ 已修复　Anthropic stream 初始 content-block 内容保留（dd8e4d3）
7. ✅ 已修复　Google history conversion signed blocks 保留（dd8e4d3）
8. ⛔ 不适用　agent session 消息队列在 compact 期间的处理（worker 架构天然排队）
9. ✅ 已修复　无效 settings 文件的 TUI 警告显示（c097dce）
10. ✅ 已修复　UTF-8 BOM 处理（c097dce）

### 中优先级（Bug 修复 + 新功能）
状态标注：✅ 已实现　◻️ 结构化/证据性 N/A（无需改动）　⏸ 将来项　❌ 决策不实现
1. compaction/summary 不暴露 tools — ◻️ 结构化 N/A（prux 压缩/摘要走服务端工具，无 tools 透传）
2. 大工具结果跨阈值压缩 — ✅ 已实现既有
3. 并发会话安全性 — ◻️ 结构化 N/A（会话文件 tempfile 唯一，无路径竞争）
4. 导入会话不覆盖同名会话 — ✅ 已实现（D1：`uniquify_import_destination` + `copy_no_overwrite`，重名递增 `name-N.jsonl`）
5. Persistent Claude thinking effort — ⏸ 部分：D2 branch summary 上限已实现（`branch_summary_max_tokens`，min(4096, model.max_tokens)）；/thinking 持久化语义保持“每次改动写全局默认”，已知分歧；managed-effort（supportsMidConvoEffort）逐轮 effort 完整移植为将来项（D3 C’）
6. Extension UI prompt events — ❌ 决策不实现（N/A：prux 扩展是 Rust 内置 plan_mode，唯一宿主 TUI 自身知晓面板状态，无事件消费者）
8. Terminal capability overrides — ❌ 决策不实现 settings 层（保持 env-only `PRUX_HYPERLINKS`/`PRUX_TRUE_COLOR`；images 依赖渲染层实现，⏸ 将来项）
10. Auth readiness checks — ✅ 已实现（D9：`auth check` oauth 分支非 `--no-refresh` 时真实刷新 `ensure_oauth_valid`；刷新失败 → not_ready/oauth_refresh_failed）
11. Compaction 竞争条件修复 — ✅ 组合落地：◻️ worker 串行 + `&mut Agent` 无并发竞争（manual vs threshold）+ ✅ D5 陈旧 usage 时间戳守卫（usage 锚点早于最近 compaction boundary 则跳过）+ ✅ D4 length/overflow 统一重试（移除失败 assistant 消息、单次重试守卫 `overflow_recovery_attempted`、溢出模式对齐 pi overflow.ts）
12. Branch summaries reasoning 输出上限处理 — ✅ 已实现（D2：branch summary max_tokens 钳制，测试通过）

### 低优先级（功能增强）
1. Restorable in-memory sessions
2. Configurable default tools
3. Constrained tool sampling
4. Qwen Token Plan Individual 模型目录
