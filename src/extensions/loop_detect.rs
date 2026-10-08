//! `loop-detect` 扩展：纯检测类扩展。
//!
//! 实时检测模型自身的**循环**行为（同一内容 / 同一动作在自我重复），命中即终止整个 run，
//! 并在聊天区发一条系统消息说明终止理由。本扩展不向会话写入任何模型可见内容，
//! 也不改写 / 检索历史消息：
//!
//! | 检测器 | 触发条件 | 动作 |
//! |--------|----------|------|
//! | thinking 字符级循环 | 思考块结尾出现两段相邻的相同 ≥thinkingWindow 字符 | 终止 run |
//! | output 字符级循环 | 可见回复同上（outputWindow 起） | 终止 run |
//! | thinking/output 语义循环 | 同一段落指纹在流内出现 semanticThreshold 次 | 终止 run |
//! | 跨轮停滞 | 最近 stagnationWindow 轮思考两两 ≥stagnationThreshold 相似 | 终止 run |
//! | 工具调用序列循环 | 相邻窗口的调用序列完全重复 | 终止 run |
//!
//! - **终止**：扩展无权直接 abort，本扩展经 `ExtensionUiRequest::AbortRun` 请求 TUI
//!   丢弃当前回合（`AgentCommand::Abort`）；回合 future 被丢弃，循环产生的半截消息不会落库。
//! - **不改写上下文**：检测只看刚产生的新消息（流式增量 / 当次工具调用）与扩展自有的
//!   有界跨轮滑窗，既不读取也不修改历史消息；`Extension::transform_context` 仅作为
//!   每轮一次的 run 上电探针。
//! - **终止理由**：经 `ExtensionUiRequest::Notify` 发到聊天区（系统消息，不落库、
//!   不进模型上下文），文本由代码按检测器生成。
//!
//! 配置持久化在 `agent_dir()/extensions/loop-detect.json`（首次加载自动创建并回填缺省键）。任意检测器把对应键设为 `0` 即关闭。
//! `/loop-detect` 打开扩展设置面板（无参数；预设档位、Space/Enter 循环切换、立即落盘），`/loop-detect reset` 清状态。
//!
//! 实现拆分为两个子模块：
//! - [`config`]：配置缺省 / 校验、面板档位映射与落盘；
//! - [`detect`]：检测状态机、各类循环算法、事件处理与扩展主体。

mod config;
mod detect;

pub use detect::LoopDetect;
