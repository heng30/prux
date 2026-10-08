# 上下文压缩（Compaction）

LLM 的上下文窗口是有限的。当对话过长时，prux 会用「压缩」把较早的内容总结成结构化摘要，同时保留最近的工作。本页介绍 prux 的自动压缩、手动压缩与分支摘要。

## 触发时机

prux 有三种压缩触发方式，对应不同原因（`reason`）：

| 触发方式 | 原因 | 说明 |
|----------|------|------|
| 自动压缩 | `threshold` | 上下文接近窗口上限时，在每轮用户输入处理前自动触发 |
| 手动压缩 | `manual` | `/compact [instructions]` |
| 溢出恢复 | `overflow` | 上下文溢出（prompt too long）后强制压缩并重试一轮 |

### 自动压缩

在每轮用户输入注入、进入 agent 主循环之前，prux 会先估算当前上下文 token。当满足：

```
contextTokens > contextWindow - reserveTokens
```

时触发压缩，为模型的回复预留空间（`reserveTokens`，默认 16384）。若 `autoCompact` 设为 `false`，自动压缩被禁用，但 `/compact` 仍可手动触发。

为避免压缩刚完成后、以旧的 usage 数据再次误触发压缩，prux 带有一个「陈旧 usage 守卫」：当估算所依赖的最后一条带 usage 的 assistant 消息时间戳早于或等于最近一次压缩边界时，会跳过本次阈值判断。

### 手动压缩

在输入框输入 `/compact` 即可手动压缩；可带自定义摘要指令聚焦重点：

```
/compact 侧重 API 变更
```

自定义指令会作为额外指令附加给摘要 prompt。压缩期间 TUI 状态栏显示 `Compacting context... (Esc to cancel)`，可用 `Esc` 中断。若无内容可压缩，会提示 `Nothing to compact`。

### 溢出恢复

当某轮回复因上下文溢出而失败（`stopReason=error` 且文案为溢出特征）时，prux 会从重试上下文移除这条失败/截断的 assistant 消息（会话历史中保留），执行一次 `overflow` 压缩，再重试一轮。若重试后仍溢出，则转达失败，不再二次压缩。

## 工作原理

1. **定位前次摘要**：找到会话中第一条 `compactionSummary` 消息，其后为待压缩的起点；若存在前次摘要，会把它作为 `<previous-summary>` 增量更新上下文（使用 UPDATE 版本的摘要 prompt）。
2. **找切点**：从最新消息往回累积 token 估算（跳过 `toolResult`），直到达到 `keepRecentTokens`（默认 20000）。若切点落在 assistant 回复内部，则回溯到该轮开头的 user 消息——压缩始终以「完整轮次」为单位，保证上下文不以半截回复开头。
3. **生成摘要**：调用模型，用结构化格式总结待压缩消息；若扩展通过 `custom_summarize` 提供了自定义摘要，则用它替代内置摘要生成。
4. **追加条目**：把摘要写入会话的 `Compaction` 条目，同时**内联保留尾部消息**（retained tail）。压缩边界随条目内联保存，fork 分支也不会丢失。
5. **重建上下文**：下轮请求的上下文 = 摘要消息 + 保留的尾部消息。聊天区保留压缩前的历史（不截断，footer 的累计 in/out/费用仍从完整历史统计）；footer 的上下文占用则立即改用压缩后的估算（见下节）。

prux 的压缩入口为 `core::compaction` 模块与 `agent_session` 的 `maybe_compact` / `maybe_compact_with`。

## 摘要格式

prux 的压缩用下面这套结构化格式；**分支摘要用不含 `## Critical Context` 段的变体**（其余段落相同，末尾同样附 `<read-files>` / `<modified-files>`）：

```markdown
## Goal
[用户想达成什么]

## Constraints & Preferences
- [用户提到的约束或偏好]

## Progress
### Done
- [x] [已完成的任务]

### In Progress
- [ ] [进行中的工作]

### Blocked
- [阻塞项，如有]

## Key Decisions
- **[决策]**: [理由]

## Next Steps
1. [接下来该做什么]

## Critical Context
- [继续所需的数据/引用]

<read-files>
path/to/file1.rs
</read-files>

<modified-files>
path/to/changed.rs
</modified-files>
```

### 消息序列化

摘要前，消息会先被序列化为纯文本（`serialize_conversation`），避免模型把对话当成要继续的会话：

```
[User]: 用户说的话
[Assistant thinking]: 内部推理
[Assistant]: 回复文本
[Assistant tool calls]: read(path="foo.rs"); edit(path="bar.rs", ...)
[Tool result]: 工具输出
```

工具结果在序列化时会被截断到 2000 字符，超出部分以标记替代，控制摘要请求的 token 预算（工具结果通常是上下文里最大的部分）。

### Token 估算

无精确 usage 时，prux 用 **tiktoken BPE 真实计数**（按模型选词表，非 OpenAI 模型回退 cl100k）：文本、思考内容与工具调用参数都逐段计数；图片没有公开 BPE 口径，按固定 **1200 token** 当量（≈4800 字符）计。TUI 底部也会显示基于该估算的上下文占用百分比。

有精确 usage 时，以「最近一条能描述当前上下文的 assistant usage」为锚点，再加上其后消息的估算。判定锚点有效性的规则是：若某条消息的时间戳晚于该 assistant（典型是压缩时插入的 `compactionSummary`），说明它是在这条回复之后才进入前缀的，该 usage 统计的是插入前的旧上下文，不能再用作锚点——此时忽略它继续向后找；全部失效则整段走纯估算。因此压缩后 `/compact` 提示里的 `estimatedTokensAfter` 是摘要 + 保留尾部的纯估算，会明显小于 `tokensBefore`。TUI 也随之把 footer 的窗口占用改为 `estimatedTokensAfter`（压缩摘要本身不带 usage，否则占用会一直停在压缩前的陈旧 usage 上）；待下一条带可用 usage 的 assistant 落地，占用自动回到 usage 锚点。切换会话 / 树导航 / `/new` 会丢弃该覆盖值。

### 文件操作累积跟踪

压缩和分支摘要都会累积跟踪文件操作。生成摘要时，prux 从被压缩的消息中提取 assistant 的 `read`/`write`/`edit` 工具调用，并叠加前一次压缩 `details` 里的文件列表。最终 `readFiles` 只包含未被修改过的读取；`modifiedFiles` = `edited` + `written`（排序）。这样多次压缩能保留完整的读写文件历史。

## 分支摘要（Branch Summarization）

导航到另一分支（核心的 `Agent::navigate_tree`）时，prux 可为被放弃的分支生成摘要并注入新分支，保留离开分支的上下文。注意：当前 TUI 的 `/tree` 只是只读文本视图（见 [sessions.md](sessions.md)），交互式导航尚未实现，因此这条路径只在 SDK / 嵌入方直接调用 `navigate_tree` 时才会走到。过程：

1. 找到新旧位置最近的公共祖先；
2. 收集从旧叶子回溯到公共祖先的被放弃条目（跳过 `toolResult`），同时从其中的嵌套 `compaction` / `branch_summary` 条目叠加文件操作；
3. 用 `BRANCH_SUMMARY_PROMPT` 生成结构化摘要，附加分支摘要前言后，作为 `BranchSummary` 条目追加到导航目标；
4. 新分支上下文包含该摘要，继续从目标继续工作。

## 扩展钩子

prux 通过扩展 trait 暴露三个压缩钩子（见 [extensions.md](extensions.md)）：

- `on_before_compact(tokens_before, reason)`：压缩开始、生成摘要前回调；
- `custom_summarize(system_prompt, conversation, summary_instructions)`：返回 `Some(摘要)` 时替代内置摘要生成；
- `on_compact_failure(reason)`：内置或自定义摘要生成失败时回调。

此外，prux 在事件流中发出 `compaction_start` / `compaction_end`（含 `tokensBefore`、`estimatedTokensAfter`、`reason` 等）事件，扩展可经 `on_agent_event` 订阅。压缩成功的 `compaction_end` 是 TUI 刷新上下文占用的依据（手动 / 阈值 / 溢出三条路径共用）。

## 设置

压缩参数位于 `~/.prux/settings.json`（或 `$PRUX_AGENT_DIR/settings.json`），使用**顶层键**：

```json
{
  "autoCompact": true,
  "compactReserveTokens": 16384,
  "compactKeepRecentTokens": 20000
}
```

| 设置 | 默认值 | 说明 |
|------|--------|------|
| `autoCompact` | `true` | 启用自动压缩；`false` 时 `/compact` 仍可用 |
| `compactReserveTokens` | `16384` | 为模型回复预留的 token |
| `compactKeepRecentTokens` | `20000` | 保留不压缩的最近 token |

`autoCompact` 也可在 `/settings` 面板的 Auto-compact 项切换，切换即写盘并持久化。
