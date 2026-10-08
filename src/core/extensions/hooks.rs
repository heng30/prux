//! 事件钩子类型。
//!
//! 扩展通过 [`super::Extension`] 的 `hooks()` 声明关心的钩子，
//! 再覆写对应的 trait 方法（`transform_context` / `before_tool_call` / `after_tool_call`）提供实现；
//! 分发逻辑按声明精确调用。

/// 扩展的事件钩子注册
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtensionHook {
    /// LLM 调用前修改上下文
    TransformContext,
    /// 工具调用前拦截
    BeforeToolCall,
    /// 工具调用后处理
    AfterToolCall,
    /// agent 内核事件流入（`on_agent_event`）。该面**高频**（流式 message_update 等），
    /// 未声明的扩展在 `dispatch_agent_event` 中会被直接跳过。
    AgentEvent,
    /// 停靠面板内容采集（`dock_lines`）。TUI 每帧采集，未声明的扩展不会被轮询。
    Dock,
    /// 覆盖层内容采集与按键回传（`overlay_view` / `on_overlay_event`）。
    /// 与 `Dock` 同样每帧采集；未声明的扩展不会被轮询、也收不到按键。
    Overlay,
    /// 用户 Enter 提交前的输入拦截（`on_user_prompt`）。空闲提交与忙碌时的
    /// steer 提交都会触发；未声明的扩展不会被询问。
    /// 同一 hook 还用于提交的**广播通知**（`on_user_submit`）：提交路径
    /// （含 Alt+Enter follow-up）会给每个声明者发一份，不要求扩展先认领输入。
    UserPrompt,
    /// 扩展↔扩展事件总线订阅（`on_extension_event`）。**订阅 = 声明本 hook**：
    /// 未声明的扩展不会被投递。见 [`super::events`]。
    ExtensionEvent,
    /// 输入框 `@` 候选来源（`suggestions`）。未声明的扩展不会被轮询。
    Suggestions,
    /// 含 system 消息的完整 transcript 变换（`transform_context_with_system`）：
    /// `TransformContext` 之后、每次 LLM 调用前投递一次；结果**原样发送**（处理器拥有
    /// 提示词与消息列表）。未声明的扩展不会被询问。
    ContextWithSystem,
    /// 轮收尾 / 结算边界（`on_boundary`）：`turn_end`（每轮结束）与
    /// `agent_before_settle`（整轮结算前）两个时机各投一次，事件 `type` 区分。
    /// 扩展可返回 [`super::BoundaryOutcome`] 要求续跑（`continue`，可附带追加消息）、
    /// 优雅结束（`end`），或追加**投影草稿**——上下文编辑（`with_omit` /
    /// `with_content_replace`，append-only，不改写历史）与 retain-none 压缩
    /// （`with_retain_none_compaction`）；草稿落库后立即重建模型上下文，
    /// 且与 `continue` / `end` 决策无关（error / aborted 轮同样生效）。
    /// 未声明的扩展不会被询问。
    Boundary,
    /// 提示缓存保温决策（`cache_warming_decision`）：每次保温刷新前投递一次，
    /// 扩展可返回 `"warm"` / `"stop"` 覆盖成本评估结论（最后一个给出 action 的扩展生效）。
    /// 未声明的扩展不会被询问。
    CacheWarmingDecision,
    /// 供应商原始流事件（`provider_stream_event`）：每条 SSE 载荷在**归一化之前**投递一次，
    /// 携带 assistant 消息不保留的 provider 特有字段。
    /// **最高频的钩子之一**（每个流式 delta 一次）：未声明的扩展完全不被投递，声明者也应只做缓冲（别做 IO/解析）。
    ProviderStreamEvent,
}
