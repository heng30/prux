//! loop-detect 的检测层：状态机、各类循环检测算法、事件处理与扩展主体。
//!
//! 检测到循环时：
//! 1. 通过 [`ExtensionUiRequest::AbortRun`] 请求 TUI **立即**丢弃当前回合（流式循环无法等回合结束）；
//! 2. 同时置位「本回合结束后终止」，在 `turn_end` 边界返回 `end`——不依赖 UI 事件循环的
//!    往返，也不波及父 run（子代理里的命中只结束自己的 run，见 `agentScope`）。
//!
//! 并用 [`ExtensionUiRequest::Notify`] 在聊天区发一条终止理由系统消息。
//! 配置/面板/落盘见 [`super::config`]。
//!
//! # 检测器一览
//!
//! 共有四个检测器；命中后统一 `AbortRun` + 置位回合后终止 + 发系统消息，并经 [`emit_detection`] 向
//! 事件总线 / `hookCmd` / `hookLog` 广播载荷。所有检测器的输入都只是**刚产生的新内容**
//! ——流式增量、当次工具调用、扩展自有的有界跨轮滑窗——既不读取也不改写历史消息。
//!
//! | 检测器 | 触发面 | 判据 | 核心实现 |
//! | --- | --- | --- | --- |
//! | 字符级流循环 | thinking / output 的 `*_delta` | 结尾出现两段相邻的相同字符块 | [`detect_repeating_suffix`] |
//! | 语义级段落循环 | 同上 | 同一段落指纹累计出现 `semanticThreshold` 次 | [`detect_semantic_loop`] |
//! | 跨轮思考停滞 | 每轮 `message_end` 的 thinking | 最近 `stagnationWindow` 轮思考两两相似度 ≥ 阈值 | [`on_message_end`] + [`similarity`] |
//! | 工具序列循环 | `before_tool_call` | 调用指纹历史尾部构成相邻重复序列 | [`detect_sequence_repeat`] |
//!
//! ## 1. 字符级流循环 —— [`detect_repeating_suffix`]
//!
//! 判据：文本**结尾是两个相邻的相同 w 字符块**（`min_window <= w <= max_window`）。
//! 是否命中用 **Z 数组**在 O(n) 内判定：取结尾 `2*max_w` 个字符并**反转**，
//! 对其求 Z 数组；若 `z[w] >= w`，说明反转后前 w 字符与紧接着的 w 字符相等，
//! 等价于原文本最后两个 w 字符块相同（`w` 从 `min_window` 起试，取最小命中）。
//! 命中时返回**去掉最后重复块**的干净前缀（`text.chars().take(n - w)`；流式调用方目前只取
//! 循环类型，前缀留给定位用）。
//! 窗口上限由 [`max_window_for`] 放大并封顶 4000；`min_window == 0`（关闭）或
//! 文本不足 `2*min_window` 时直接跳过。
//!
//! ## 2. 语义级段落循环 —— [`detect_semantic_loop`]
//!
//! 判据：**同一段落的指纹累计出现 `threshold` 次**。按空行切段（[`find_blank_line`]），
//! 只统计长度 ≥ [`PARA_MIN_LEN`] 的段落；指纹 = 归一化后前 [`FINGERPRINT_LEN`] 个字符。
//! 归一化用 [`strip_leading_counter`] 把 `23. foo` 的有序列表序号抹成 `#. foo`，
//! 避免递增序号掩盖重复。``` 围栏内的段落不参与（围栏状态 `in_fence` 按围栏行奇偶翻转）。
//! 关键点是**增量扫描**：扫描位置 `scanned`、指纹计数 `counts`、围栏状态都保存在
//! [`SemanticState`] 里跨 delta 复用，每个段落只处理一次；且只有被空行**终结**的段落才
//! 计数（仍在增长的末段不计），避免流式过程中重复计数。命中时返回该段之前的干净前缀（同上，目前只取类型）。
//!
//! ## 3. 跨轮思考停滞 —— [`on_message_end`] + [`similarity`]
//!
//! 在每个**干净结束**的 assistant 轮次（未命中流循环）取最后一个 thinking 块文本，
//! 压入长度受 `stagnationWindow` 限制的滑窗 `thinking_history`；当窗口已满且
//! **相邻两两**相似度都不低于 `stagnationThreshold` 时判停滞（命中后清空滑窗，避免复触发）。
//! 相似度 [`similarity`] 先过最小词数门槛 [`MIN_WORDS_FOR_SIMILARITY`]（任一侧词太少即
//! “不可比”，返回 `None`），再用 [`jaccard`]（小写空格切词的词集 Jaccard）计算，
//! 以免 “Let me read the file.” 这类短思考块虚高相似度误伤。
//!
//! ## 4. 工具序列循环 —— [`check_tool_call`] / [`detect_sequence_repeat`]
//!
//! 每个工具调用取指纹 `name:{stable json}`（[`hash_tool_call`]）；命中判据是
//! **历史尾部存在相邻重复序列**：对周期 `w = 1..=n/2`，若末 w 个指纹与紧邻的前 w 个
//! 逐一相等即为重复（取最小 w 作为周期）。仅在**放行路径**记录历史，被拦截的调用不计数；
//! 豁免工具（`toolLoopExempt`）不参与判定但仍记录，用于打断其它工具的相邻性。
//!
//! # 流式调度与状态机
//!
//! [`State::feed_update`] 消费 `message_update` 里的 `assistantMessageEvent`，按
//! `contentIndex` 区分 thinking / 可见输出两条流；遇到 `*_start` 或索引变化即重置该流
//! 的缓冲与检测器。两条流共用 [`check_stream`]，它先按 [`stream_stride`] 限频
//! （每积累约半个窗口才检一次），再**先字符级、后语义级**依次判定。命中后
//! [`State::arm_after_detection`] 置 `stream_aborted`，同一条消息的后续 delta 不再检测。
//!
//! # 上下电（为什么需要 TransformContext 探针）
//!
//! 框架按 [`interest_for`] 实时门控回调：run 空闲时只留每轮一次的
//! [`ExtensionHook::TransformContext`]；仅 `run_active` 时才订阅高频的 AgentEvent /
//! BeforeToolCall。而 run 的启动信号 `agent_start` 本身就在被门控的高频面上，若一并
//! 门控则扩展将永远收不到信号——因此用每轮 LLM 调用前的 `transform_context` 作为**上电
//! 探针**（[`begin_run`]，并补记被错过的首个 `turn_start`）。`agent_end` / `agent_settled`
//! 一律整段重置并断电；全部检测器关闭时 [`interest_for`] 返回空，扩展完全零开销。

use super::config::*;
use crate::{
    core::{
        self,
        extensions::{
            self, BeforeToolCallOutcome, BoundaryOutcome, Extension, ExtensionCommand,
            ExtensionHook, ExtensionMode, ExtensionSetting, ExtensionTool, ExtensionUiRequest,
            SubcommandDef, UiNotifyLevel,
        },
        provider::AgentMessage,
        session_manager::Session,
        tools::ToolError,
    },
    error::Result,
    extensions::{EXTENSION_FACTORIES, ExtensionFactory, PRIORITY_LOOP_DETECT, command_arg},
    modes::interactive::{app::App, handlers::register_slash_command},
    utils::time::now_iso,
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex, MutexGuard, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use strum_macros::IntoStaticStr;

/// 语义检测：短于该长度的段落不参与指纹统计
const PARA_MIN_LEN: usize = 40;
/// 语义检测：段落指纹取前 N 字符
const FINGERPRINT_LEN: usize = 60;
/// 相似度比较的最少词数：任一侧词数低于此值时视为“不可比”（避免短思考块被误判停滞）
const MIN_WORDS_FOR_SIMILARITY: usize = 5;

/// 子命令候选元数据（与 `command_loop_detect` 的 `reset` 分支一一对应，
/// 一致性由 `loop_detect_subcommands_match_parser` 验证）。
///
/// 无参 = 开关设置面板（面板本身已展示/可改全部配置），所以不声明 `status`：
/// 面板与兜底的状态输出看到的是同一份配置。
const SUBCOMMANDS: &[SubcommandDef] = &[SubcommandDef {
    name: "reset",
    description: "Clear detection state (history and counters)",
}];

/// 每次检测为真时置位、用于「每会话只警告一次」的外部 hook 失败提示（`hookCmd` 启动失败）
static HOOK_WARNED: AtomicBool = AtomicBool::new(false);
/// 同上，用于 `hookLog` 写入失败提示
static LOG_WARNED: AtomicBool = AtomicBool::new(false);

/// 扩展工厂：编译期注册到全局扩展表（[`EXTENSION_FACTORIES`]）
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static LOOP_DETECT_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_LOOP_DETECT,
    make: || -> Arc<dyn Extension> { Arc::new(LoopDetect) },
};

/// 获取全局状态锁；从 poison 中恢复（一次检测 panic 不应永久禁用扩展）。
fn lock_state() -> MutexGuard<'static, State> {
    state().lock().unwrap_or_else(|e| e.into_inner())
}

/// 复位全局检测状态（run 结束 / 会话切换 / `/loop-detect reset` 共用）。
fn reset_all() {
    lock_state().reset();
}

/// loop-detect 扩展主体。
pub struct LoopDetect;

impl Extension for LoopDetect {
    /// 扩展标识 `loop-detect`（同时用作配置文件名与命令名前缀）。
    fn name(&self) -> &str {
        EXT
    }

    /// 面板展示的说明：检测思考/输出/工具调用循环并尽快终止 run。
    fn description(&self) -> &str {
        "Detect thinking/output/tool-call loops and terminate the run as soon as one is found."
    }

    /// 仅在 Dev / Creator 模式下可用（调试用途）。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Dev, ExtensionMode::Creator]
    }

    /// 默认关闭：命中会直接终止 run，需用户显式启用。
    fn default_enabled(&self) -> bool {
        false
    }

    /// 不注册任何工具：检测完全由事件回调驱动。
    fn tools(&self) -> Vec<ExtensionTool> {
        Vec::new()
    }

    /// 注册 `/loop-detect`：无参打开设置面板，`reset` 清空检测状态。
    fn commands(&self) -> Vec<ExtensionCommand> {
        vec![ExtensionCommand {
            name: CMD.to_string(),
            description: "Open the loop-detect settings panel; /loop-detect reset to clear state"
                .to_string(),
            busy_safe: true, // handler 只改扩展内存态 / 写配置文件，不锁 agent，忙碌安全
            subcommands: SUBCOMMANDS.to_vec(),
        }]
    }

    /// 面板设置项：预设档位 + Space/Enter 循环（无自由输入）。
    fn settings(&self) -> Vec<ExtensionSetting> {
        let st = lock_state();
        panel_settings(&st.cfg)
    }

    /// 应用面板选择并立即落盘（面板是"设置"语义，不像 `set` 那样仅会话内）。
    fn apply_setting(&self, key: &str, value: &str) -> std::result::Result<(), String> {
        {
            let mut st = lock_state();
            apply_panel_choice(&mut st.cfg, key, value)?;
        }
        let path = config_path();
        let cfg = lock_state().cfg.clone();
        write_config(&path, &cfg);
        Ok(())
    }

    /// 注册时把 `/loop-detect` 的执行入口挂到交互模式的斜杠命令表。
    fn on_registered(&self) {
        register_slash_command(EXT, CMD, command_loop_detect);
    }

    /// 扩展被关闭时清空全部检测状态，避免残留历史影响下次启用。
    fn on_enabled_changed(&self, enabled: bool) {
        if !enabled {
            reset_all();
        }
    }

    // 实时按状态计算兴趣（单一谓词见 interest_for）：全部检测器关闭时空闲零开销；
    // run 进行中才订阅高频的 AgentEvent / BeforeToolCall。
    /// 实时按当前状态返回需要订阅的回调集合（谓词见 [`interest_for`]）。
    fn hooks(&self) -> Vec<ExtensionHook> {
        interest_for(&lock_state())
    }

    /// 边界：命中过检测器时，在 `turn_end` 返回 `end`（本 run 在当前回合结束后优雅终止）。
    /// 只消费一次（`terminate_after_turn` 复位），不影响后续 run。
    fn on_boundary(&self, event: &Value) -> Option<BoundaryOutcome> {
        if event.get("type").and_then(|v| v.as_str()) != Some("turn_end") {
            return None;
        }
        let mut st = lock_state();
        if !st.terminate_after_turn {
            return None;
        }
        st.terminate_after_turn = false;
        Some(BoundaryOutcome::end())
    }

    /// 按事件 `type` 把 agent 生命周期事件分发到各处理器（未识别类型忽略）。
    fn on_agent_event(&self, event: &Value) {
        match event.get("type").and_then(|v| v.as_str()) {
            Some("agent_start") => on_agent_start(),
            Some("turn_start") => on_turn_start(),
            Some("message_start") => on_message_start(event),
            Some("message_update") => on_message_update(event),
            Some("message_end") => on_message_end(event),
            Some("agent_end") => on_agent_end(),
            // 整轮（含重试/溢出恢复的所有 run_loop 尝试）真正结束
            Some("agent_settled") => on_agent_settled(),
            _ => {}
        }
    }

    /// 记录会话工作目录与 id/文件路径，并整段复位检测状态。
    fn on_session_start(&self, cwd: &str, _messages: &[AgentMessage], session: Option<&Session>) {
        {
            let mut st = lock_state();
            st.cwd = cwd.to_string();
            if let Some(s) = session {
                st.session_id = s.session_id.clone();
                st.session_file = s
                    .session_file
                    .as_ref()
                    .map(|p| p.to_string_lossy().to_string());
            }
        }
        reset_all();
    }

    /// 切换到目标会话：更新会话路径/id/cwd 后整段复位检测状态。
    fn on_session_switched(&self, session_path: Option<&str>, _messages: &[AgentMessage]) {
        {
            let mut st = lock_state();
            st.session_file = session_path
                .filter(|p| !p.is_empty())
                .map(|p| p.to_string());

            if let Some(path) = session_path.filter(|p| !p.is_empty())
                && let Ok(s) = Session::open(path)
            {
                st.session_id = s.session_id.clone();
                st.cwd = s.cwd.clone();
            }
        }
        reset_all();
    }

    /// LLM 调用前的 run 上电探针。
    ///
    /// 本回调**不改写上下文**：死循环检测只针对刚产生的新消息（流式增量 / 当次工具调用 /
    /// 扩展自有的有界跨轮滑窗），既不读取也不修改历史消息。这里只借"每轮 LLM 调用前
    /// 调用一次"这一时机给扩展上电——`agent_start` 落在被门控的高频 `AgentEvent` 面上，
    /// 若不在低频面上上电，扩展将永远收不到启动信号（见 [`interest_for`]）。
    fn transform_context(&self, _messages: &mut Vec<AgentMessage>) -> Result<()> {
        let mut st = lock_state();
        if !st.run_active {
            begin_run(&mut st);
            // 上电发生在 agent_start 之后，首个 turn_start 已被错过：在此补齐。
            st.turn_index += 1;
        }
        Ok(())
    }

    /// 工具调用前拦截：工具面只有“调用序列完全重复”一个检测器。
    ///
    /// 命中即 `block: true`（唯一能阻止该调用执行的机制）并请求 [`ExtensionUiRequest::AbortRun`]，
    /// 终止整个 run；同时先发一条系统消息说明理由。`reason` 复用同一条终止理由。
    fn before_tool_call_ext(
        &self,
        name: &str,
        args: &Value,
        _assistant_message: Option<&AgentMessage>,
        _context: &[AgentMessage],
        _all_args: &Value,
    ) -> std::result::Result<BeforeToolCallOutcome, ToolError> {
        let hit = check_tool_call(&mut lock_state(), name, args);
        let Some(hit) = hit else {
            return Ok(BeforeToolCallOutcome::allow());
        };

        emit_detection(
            "tool_loop",
            json!({ "toolName": name, "windowSize": hit.window_size }),
        );

        notify_text(&hit.reason, UiNotifyLevel::Warning);
        request_termination();

        Ok(BeforeToolCallOutcome {
            block: true,
            reason: Some(hit.reason),
            terminate: false,
            args: None,
        })
    }
}

/// 单一“分发兴趣”谓词（对齐 plan-mode）：框架在分发每个回调前按 [`Extension::hooks`] 门控，
/// 这里集中判定当前状态关心哪些回调。采用**实时求值**（不缓存掩码）。
///
/// loop-detect 的输入是 agent 流式事件，必须能在 run 开始前后“上电”。而 run 启动信号
/// （`agent_start`）本身就在高频的 [`ExtensionHook::AgentEvent`] 面上：若把它一并门控，
/// 扩展将永远收不到启动信号而彻底失联。因此保留每轮仅一次的
/// [`ExtensionHook::TransformContext`] 作为上电探针，
/// 仅 run 进行中（`run_active`）才订阅高频的 AgentEvent 与工具前的 BeforeToolCall。
fn interest_for(st: &State) -> Vec<ExtensionHook> {
    if !any_detector_enabled(&st.cfg) {
        return Vec::new();
    }

    let mut out = vec![ExtensionHook::TransformContext];

    // 边界：命中后由 `turn_end` 边界收尾。run 进行中（或已有未消费的终止标志）才订阅：
    // 结算边界在 run 结束（agent_end 复位）之后才投递，此处订阅会让内核为**每次 prompt**
    // 序列化整份 transcript 构造结算事件，而本扩展只关心 run 内的 `turn_end`。
    if st.run_active || st.terminate_after_turn {
        out.push(ExtensionHook::Boundary);
    }

    if st.run_active {
        out.push(ExtensionHook::AgentEvent);
        out.push(ExtensionHook::BeforeToolCall);
    }
    out
}

/// 字符级检测的扫描上限：随窗口放大，封顶 4000
fn max_window_for(window: usize) -> usize {
    (window * 50).min(4000)
}

/// 流式检测的步长：窗口的一半（10..=50）；窗口关闭（0）时用 50
fn stream_stride(window: usize) -> usize {
    if window == 0 {
        50
    } else {
        (window / 2).clamp(10, 50)
    }
}

/// 流命中来自哪条流
#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr)]
#[strum(serialize_all = "lowercase")]
enum StreamName {
    /// 思考块（`thinking_delta`）
    Thinking,
    /// 可见回复（`text_delta`）
    Output,
}

impl StreamName {
    /// 事件载荷里的流名（`thinking` / `output`）。
    fn as_str(self) -> &'static str {
        self.into()
    }
}

/// 流命中的循环类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr)]
#[strum(serialize_all = "lowercase")]
enum LoopKind {
    /// 字符级：结尾出现两段相邻的相同字符块
    Character,
    /// 语义级：同一段落指纹重复出现
    Semantic,
}

impl LoopKind {
    /// 事件载荷里的循环类型名（`character` / `semantic`）。
    fn as_str(self) -> &'static str {
        self.into()
    }
}

/// 段落指纹增量扫描态
#[derive(Default)]
struct SemanticState {
    /// 段落指纹 → 已出现次数
    counts: HashMap<String, usize>,
    /// 已扫描到的字节位置（增量扫描起点）
    scanned: usize,
    /// 是否处于代码围栏（fenced code block）内；围栏内段落不参与语义检测
    in_fence: bool,
}

impl SemanticState {
    /// 空的增量扫描态：无指纹计数、从头扫描、不在代码围栏内。
    fn new() -> Self {
        Self::default()
    }
}

/// 全局状态（单一 agent 进程；run 结束 / 会话切换均整段重置）
struct State {
    /// 运行期配置（缺省齐全，加载时合并 `loop-detect.json`）
    cfg: Config,

    // ---- 流式检测 ----
    /// 本次 assistant 消息流是否已命中并请求中止；置位后忽略后续 delta
    stream_aborted: bool,
    /// thinking 流上次执行检测时的字符长度（按步长限频）
    last_checked_len: usize,
    /// 可见回复流上次执行检测时的字符长度（按步长限频）
    last_checked_output_len: usize,
    /// 当前 thinking 块的 `contentIndex`；用于识别新块并重置检测器
    cur_thinking_index: Option<u64>,
    /// 当前可见回复块的 `contentIndex`；用于识别新块并重置检测器
    cur_text_index: Option<u64>,
    /// 当前 thinking 块全文累积（供字符级 / 语义级检测）
    thinking_buf: String,
    /// `thinking_buf` 的字符数（增量维护，避免重复扫描）
    thinking_len: usize,
    /// 当前可见回复全文累积（供字符级 / 语义级检测）
    text_buf: String,
    /// `text_buf` 的字符数（增量维护）
    text_len: usize,
    /// thinking 流的段落指纹扫描态（已扫描位置 / 指纹计数 / 围栏状态）
    thinking_sem: SemanticState,
    /// 可见回复流的段落指纹扫描态
    output_sem: SemanticState,
    /// 最近一次流命中的来源（thinking / output），用于生成终止理由
    loop_stream: StreamName,
    /// 最近一次流命中的类型（字符级 / 语义级），用于生成终止理由
    loop_kind: LoopKind,

    // ---- 跨轮 / 工具 ----
    /// 已放行工具调用的指纹序列（检测相邻窗口完全重复）
    tool_history: Vec<String>,
    /// 最近 `stagnationWindow` 轮的思考文本（两两相似度判定停滞）
    thinking_history: Vec<String>,
    /// 当前轮序号（从 1 起，写入检测载荷）
    turn_index: u64,
    /// 是否有进行中的 agent run：高频回调（AgentEvent / BeforeToolCall）的上电标志，
    /// 由每轮的 transform_context 上电、run 结束（agent_end / agent_settled）断电（见 interest_for）。
    run_active: bool,
    /// 命中后置位：`turn_end` 边界返回 `end`（本 run 在当前回合结束后优雅终止）。
    /// 与 `AbortRun` 并存：前者是本 run 内的确定性收尾，后者是交互模式下的立即丢弃。
    terminate_after_turn: bool,

    // ---- 会话元信息（检测载荷用） ----
    /// 会话工作目录（`hookLog` 相对路径按其解析，并写入检测载荷）
    cwd: String,
    /// 会话 id（写入检测载荷）
    session_id: String,
    /// 会话文件路径（写入检测载荷，可能为空）
    session_file: Option<String>,
    /// 当前模型 id（写入检测载荷；暂无写入点，载荷中 `model` 恒为 null）
    model_id: Option<String>,
    /// 当前模型名（写入检测载荷；暂无写入点）
    model_name: Option<String>,
    /// 当前模型 provider（写入检测载荷；暂无写入点）
    model_provider: Option<String>,
}

/// 进程级全局状态单例（配置 + 检测历史），所有回调经 [`lock_state`] 访问。
fn state() -> &'static Mutex<State> {
    // 进程级单例：首次访问时加载配置并构造状态
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(|| {
        Mutex::new(State {
            cfg: load_config(),
            stream_aborted: false,
            last_checked_len: 0,
            last_checked_output_len: 0,
            cur_thinking_index: None,
            cur_text_index: None,
            thinking_buf: String::new(),
            thinking_len: 0,
            text_buf: String::new(),
            text_len: 0,
            thinking_sem: SemanticState::new(),
            output_sem: SemanticState::new(),
            loop_stream: StreamName::Thinking,
            loop_kind: LoopKind::Character,
            tool_history: Vec::new(),
            thinking_history: Vec::new(),
            turn_index: 0,
            run_active: false,
            terminate_after_turn: false,
            cwd: std::env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default(),
            session_id: String::new(),
            session_file: None,
            model_id: None,
            model_name: None,
            model_provider: None,
        })
    })
}

impl State {
    /// 只清空流式检测状态（缓冲区、限频计数、段落指纹），保留跨轮与工具历史。
    fn reset_stream_state(&mut self) {
        self.last_checked_len = 0;
        self.last_checked_output_len = 0;
        self.cur_thinking_index = None;
        self.cur_text_index = None;
        self.thinking_buf.clear();
        self.thinking_len = 0;
        self.text_buf.clear();
        self.text_len = 0;
        self.thinking_sem = SemanticState::new();
        self.output_sem = SemanticState::new();
    }

    /// 清空全部检测状态（run 结束 / 会话切换 / 用户新 run；保留会话元信息与配置）
    fn reset(&mut self) {
        self.stream_aborted = false;
        self.reset_stream_state();
        self.loop_stream = StreamName::Thinking;
        self.loop_kind = LoopKind::Character;
        self.tool_history.clear();
        self.thinking_history.clear();
        self.turn_index = 0;
        self.run_active = false;
        self.terminate_after_turn = false;
    }

    /// 重置流式检测器（新 assistant 消息 / 新块；不动跨轮与工具状态）
    fn reset_stream_only(&mut self) {
        self.stream_aborted = false;
        self.reset_stream_state();
        self.loop_stream = StreamName::Thinking;
        self.loop_kind = LoopKind::Character;
    }

    /// 消费一条流式增量，命中循环时返回检测结果。
    fn feed_update(&mut self, event: &Value) -> Option<StreamHit> {
        if self.stream_aborted {
            return None;
        }
        let ev = event.get("assistantMessageEvent")?;
        let et = ev.get("type").and_then(|v| v.as_str())?;
        let idx = ev.get("contentIndex").and_then(|v| v.as_u64());

        match et {
            "thinking_start" => {
                self.cur_thinking_index = idx;
                self.thinking_buf.clear();
                self.thinking_len = 0;
                self.last_checked_len = 0;
                self.thinking_sem = SemanticState::new();
            }
            "thinking_delta" => {
                let delta = ev.get("delta").and_then(|v| v.as_str())?;
                if self.cur_thinking_index != idx {
                    // 未收到 start 或新块开始（provider 差异）：重置本流检测器
                    self.cur_thinking_index = idx;
                    self.thinking_buf.clear();
                    self.thinking_len = 0;
                    self.last_checked_len = 0;
                    self.thinking_sem = SemanticState::new();
                }
                self.thinking_buf.push_str(delta);
                self.thinking_len += char_len(delta);
                return self.check_thinking();
            }
            "text_start" => {
                self.cur_text_index = idx;
                self.text_buf.clear();
                self.text_len = 0;
                self.last_checked_output_len = 0;
                self.output_sem = SemanticState::new();
            }
            "text_delta" => {
                let delta = ev.get("delta").and_then(|v| v.as_str())?;
                if self.cur_text_index != idx {
                    self.cur_text_index = idx;
                    self.text_buf.clear();
                    self.text_len = 0;
                    self.last_checked_output_len = 0;
                    self.output_sem = SemanticState::new();
                }
                self.text_buf.push_str(delta);
                self.text_len += char_len(delta);
                return self.check_output();
            }
            _ => {}
        }
        None
    }

    /// 对 thinking 流按 `thinkingWindow`/`semanticThreshold` 跑一次检测；
    /// 命中则记录流名/类型并置位 abort，返回 `None` 表示未命中或未到检测步长。
    fn check_thinking(&mut self) -> Option<StreamHit> {
        let window = self.cfg.int("thinkingWindow");
        let threshold = self.cfg.int("semanticThreshold");
        let (kind, _) = check_stream(
            &self.thinking_buf,
            self.thinking_len,
            &mut self.last_checked_len,
            &mut self.thinking_sem,
            window,
            threshold,
        )?;
        self.loop_stream = StreamName::Thinking;
        self.loop_kind = kind;
        self.arm_after_detection()
    }

    /// 对可见输出流按 `outputWindow`/`semanticThreshold` 跑一次检测，命中处理同 [`State::check_thinking`]。
    fn check_output(&mut self) -> Option<StreamHit> {
        let window = self.cfg.int("outputWindow");
        let threshold = self.cfg.int("semanticThreshold");
        let (kind, _) = check_stream(
            &self.text_buf,
            self.text_len,
            &mut self.last_checked_output_len,
            &mut self.output_sem,
            window,
            threshold,
        )?;
        self.loop_stream = StreamName::Output;
        self.loop_kind = kind;
        self.arm_after_detection()
    }

    /// 流命中后的公共收尾：置 abort 标志（同一消息流后续 delta 不再检测）并返回命中的流/类型。
    fn arm_after_detection(&mut self) -> Option<StreamHit> {
        self.stream_aborted = true;
        Some(StreamHit {
            stream: self.loop_stream,
            kind: self.loop_kind,
        })
    }
}

/// 一次流命中的检测结果（供 [`handle_stream_hit`] 生成终止理由）
struct StreamHit {
    /// 命中的流（thinking / output）
    stream: StreamName,
    /// 命中的循环类型（字符级 / 语义级）
    kind: LoopKind,
}

/// run 级启动：给扩展“上电”（run 结束 / 会话切换已把状态整段重置，这里幂等）。
fn begin_run(st: &mut State) {
    st.reset();
    st.run_active = true;
    st.terminate_after_turn = false;
}

/// 命中统一的终止请求：立即丢弃当前回合（交互模式）+ 置位回合后终止（边界 `end`）。
///
/// 两条路互补：`AbortRun` 由 TUI 事件循环异步处理（且会中止整个 worker 的 run），
/// 边界 `end` 在 agent 内核里确定性生效——即使 UI 请求被丢弃/迟到（或命中发生在子代理 run 里），
/// 本 run 也会在当前回合结束后结束。
fn request_termination() {
    lock_state().terminate_after_turn = true;
    extensions::request_ui(ExtensionUiRequest::AbortRun);
}

/// agent_start 正常会被门控挡在高频面之外（见 interest_for）；保留此入口用于
/// “上一轮 run 缺 agent_settled（异常退出）”后重新上电，幂等复位。
fn on_agent_start() {
    begin_run(&mut lock_state());
}

/// 会话结束（`agent_end`）：整段重置，保证下一次 run 从初始状态开始。
///
/// `agent_end` 每次 run_loop 尝试结束都会发（含自动重试/溢出恢复的中间态），
/// 这里一律复位；下一次 LLM 调用前由 [`Extension::transform_context`] 探针重新上电。
fn on_agent_end() {
    reset_all();
}

/// 整轮真正结束（`agent_settled`）：同 [`on_agent_end`]，整段重置并断电。
fn on_agent_settled() {
    reset_all();
}

/// 轮次开始：轮序号 +1 并复位流式检测器（不动跨轮与工具历史）。
fn on_turn_start() {
    let mut st = lock_state();
    st.turn_index += 1;
    st.reset_stream_only();
}

/// 新 assistant 消息开始：复位流式检测器；非 assistant 角色直接忽略。
fn on_message_start(event: &Value) {
    let role = event
        .get("message")
        .and_then(|m| m.get("role"))
        .and_then(|r| r.as_str())
        .unwrap_or("");
    if role != "assistant" {
        return;
    }
    let mut st = lock_state();
    st.reset_stream_only();
}

/// 消费一条流式增量，命中循环时交由 [`handle_stream_hit`] 收尾。
fn on_message_update(event: &Value) {
    let hit = {
        let mut st = lock_state();
        st.feed_update(event)
    };
    if let Some(hit) = hit {
        handle_stream_hit(hit);
    }
}

/// 流命中收尾：广播检测事件、发系统消息并请求终止当前 run。
fn handle_stream_hit(hit: StreamHit) {
    let event_name = match (hit.stream, hit.kind) {
        (StreamName::Thinking, LoopKind::Character) => "thinking_loop",
        (StreamName::Thinking, LoopKind::Semantic) => "semantic_loop",
        (StreamName::Output, LoopKind::Character) => "output_loop",
        (StreamName::Output, LoopKind::Semantic) => "output_semantic_loop",
    };
    emit_detection(
        event_name,
        json!({ "stream": hit.stream.as_str(), "kind": hit.kind.as_str() }),
    );
    let reason = reason_stream(&lock_state().cfg, hit.stream, hit.kind);
    notify_text(&reason, UiNotifyLevel::Warning);
    // 终止整个 run：TUI 丢弃当前回合（回合 future 被 drop，半截消息不落库），
    // 并在 `turn_end` 边界返回 `end`（UI 请求被丢弃/迟到时的确定性收尾）。
    request_termination();
}

/// assistant 消息结束：先复位流状态，再判定跨轮思考停滞
/// （滑窗已满且相邻两两相似度均不低于阈值即命中）。
fn on_message_end(event: &Value) {
    let Some(message) = event.get("message") else {
        return;
    };
    if message.get("role").and_then(|r| r.as_str()) != Some("assistant") {
        return;
    }
    let thinking = extract_last_thinking(message);

    // 中止已生效时该消息通常不会落库（回合 future 被丢弃）；若仍走到这里，只复位流状态。
    if lock_state().stream_aborted {
        let mut st = lock_state();
        st.stream_aborted = false;
        st.reset_stream_state();
        return;
    }

    // 干净轮：复位流检测器，再判定跨轮停滞。
    lock_state().reset_stream_state();

    let stagnation = {
        let mut st = lock_state();
        let window = st.cfg.int("stagnationWindow");
        if window == 0 {
            None
        } else if let Some(t) = thinking.filter(|t| !t.is_empty()) {
            st.thinking_history.push(t);
            while st.thinking_history.len() > window {
                st.thinking_history.remove(0);
            }

            if st.thinking_history.len() >= window
                && st.thinking_history.windows(2).all(|w| {
                    similarity(&w[0], &w[1]).is_some_and(|s| s >= st.cfg.num("stagnationThreshold"))
                })
            {
                let threshold = st.cfg.num("stagnationThreshold");
                // 命中后重置滑窗，避免后续轮次反复触发
                st.thinking_history.clear();
                let reason = reason_stagnation(window, threshold);
                Some((window, threshold, reason))
            } else {
                None
            }
        } else {
            None
        }
    };

    if let Some((window, threshold, reason)) = stagnation {
        emit_detection(
            "stagnation",
            json!({ "window": window, "threshold": threshold }),
        );
        notify_text(&reason, UiNotifyLevel::Warning);
        request_termination();
    }
}

/// 工具序列循环的命中结果
struct ToolHit {
    /// 重复序列的周期（相邻窗口大小）
    window_size: usize,
    /// 终止理由（同时作为聊天区系统消息与 block reason）
    reason: String,
}

/// 检测"工具调用序列完全重复"：命中返回详情；未命中则记录本次调用并返回 `None`。
///
/// 只在放行路径上记录历史：被拦截的调用不消耗预算。豁免工具（`toolLoopExempt`）
/// 不参与判定，但仍记录，用于打断其它工具调用的相邻性。
fn check_tool_call(st: &mut State, name: &str, args: &Value) -> Option<ToolHit> {
    if st.cfg.int("toolLoop") == 0 {
        return None;
    }
    let hash = hash_tool_call(name, args);
    if !is_exempt_tool(name, &st.cfg.text("toolLoopExempt")) {
        let mut probe = st.tool_history.clone();
        probe.push(hash.clone());
        let window_size = detect_sequence_repeat(&probe);
        if window_size > 0 {
            return Some(ToolHit {
                window_size,
                reason: reason_tool_loop(name, window_size),
            });
        }
    }
    st.tool_history.push(hash);
    None
}

/// 构造检测载荷（字段为对外公开契约，勿改名）。
fn build_payload(st: &State, event: &str, details: Value) -> Value {
    let model = match (&st.model_id, &st.model_name, &st.model_provider) {
        (Some(id), name, provider) => json!({ "id": id, "name": name, "provider": provider }),
        _ => Value::Null,
    };
    json!({
        // prux 事件分发按 `type` 匹配：其它扩展订阅 "loop-detect:detection"；
        // `event` 为具体检测器名。
        "type": "loop-detect:detection",
        "event": event,
        "timestamp": now_iso(),
        "model": model,
        "sessionId": st.session_id,
        "sessionFile": st.session_file,
        "cwd": st.cwd,
        "turnIndex": st.turn_index,
        "details": details,
    })
}

/// 向三条观察通道广播一次检测：扩展事件总线（总是）、`hookCmd`、`hookLog`。
fn emit_detection(event: &str, details: Value) {
    let (payload, hook_cmd, hook_log, timeout_ms) = {
        let st = lock_state();
        (
            build_payload(&st, event, details),
            st.cfg.text("hookCmd"),
            st.cfg.text("hookLog"),
            st.cfg.int("hookTimeoutMs").max(1) as u64,
        )
    };
    let line = serde_json::to_string(&payload).unwrap_or_default();

    // 扩展事件总线：其它扩展经 on_agent_event 订阅 "loop-detect:detection"。
    core::extensions::dispatch_agent_event(&payload);

    if !hook_cmd.trim().is_empty() {
        spawn_hook(&hook_cmd, &line, timeout_ms);
    }

    if !hook_log.trim().is_empty() {
        append_log(&hook_log, &line);
    }
}

/// 外部命令，fire-and-forget；JSON 载荷作为最后一个参数，超时强杀。
fn spawn_hook(cmd: &str, payload: &str, timeout_ms: u64) {
    let parts: Vec<&str> = cmd.split_whitespace().collect();
    if parts.is_empty() {
        return;
    }

    let mut command = Command::new(parts[0]);
    command
        .args(&parts[1..])
        .arg(payload)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    match command.spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_millis(timeout_ms);
                loop {
                    match child.try_wait() {
                        Ok(Some(_)) => break,
                        Ok(None) => {
                            if Instant::now() >= deadline {
                                _ = child.kill();
                                _ = child.wait();
                                break;
                            }
                            std::thread::sleep(Duration::from_millis(50));
                        }
                        Err(_) => break,
                    }
                }
            });
        }
        Err(err) => {
            if !HOOK_WARNED.swap(true, Ordering::Relaxed) {
                notify_text(
                    &format!("loop-detect: hookCmd failed — {err}"),
                    UiNotifyLevel::Warning,
                );
            }
        }
    }
}

/// `hookLog`：按行追加 JSONL（相对路径按会话 cwd 解析）。
fn append_log(log: &str, line: &str) {
    let path = if Path::new(log).is_absolute() {
        PathBuf::from(log)
    } else {
        Path::new(&lock_state().cwd).join(log)
    };

    let result = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut f| writeln!(f, "{line}"));

    if result.is_err() && !LOG_WARNED.swap(true, Ordering::Relaxed) {
        notify_text(
            &format!("loop-detect: cannot write hookLog — {}", path.display()),
            UiNotifyLevel::Warning,
        );
    }
}

/// 往聊天区发一条系统通知（扩展 UI 请求，不阻塞调用方）。
fn notify_text(text: &str, level: UiNotifyLevel) {
    extensions::request_ui(ExtensionUiRequest::Notify {
        text: text.to_string(),
        level,
    });
}

/// `/loop-detect` 命令处理：无参切换设置面板，`reset` 清空状态，其余输出配置与运行状态。
/// 返回值表示是否退出程序（此命令恒为 `false`）。
fn command_loop_detect(app: &mut App, raw: &str) -> bool {
    let trimmed = command_arg(raw).trim();

    // 无参数 = 打开/关闭设置面板
    if trimmed.is_empty() {
        if app.ext_settings.is_open_for(EXT) {
            app.ext_settings.close();
        } else {
            app.ext_settings.open(EXT);
        }
        app.dirty = true;
        return false;
    }

    if trimmed == "reset" {
        reset_all();
        notify_text("loop-detect: state reset", UiNotifyLevel::Info);
        return false;
    }

    // 状态
    let st = lock_state();
    let cfg = &st.cfg;
    let mut lines = vec![
        "loop-detect status".to_string(),
        format!("  run active:          {}", st.run_active),
        format!("  stream aborted:      {}", st.stream_aborted),
        format!("  tool history:        {} calls", st.tool_history.len()),
        format!(
            "  stagnation history:  {}/{} turns",
            st.thinking_history.len(),
            cfg.int("stagnationWindow")
        ),
        String::new(),
        "  config (edit loop-detect.json or use the /loop-detect panel):".to_string(),
    ];
    for (key, _) in NUMERIC_DEFAULTS {
        lines.push(format!("    {key}={}", cfg.num(key)));
    }
    for (key, _) in STRING_DEFAULTS {
        lines.push(format!("    {key}=\"{}\"", cfg.text(key)));
    }
    notify_text(&lines.join("\n"), UiNotifyLevel::Info);
    false
}

/// 字符数（按 Unicode 标量计，而非字节数）。
fn char_len(s: &str) -> usize {
    s.chars().count()
}

/// 终止理由：字符级 / 语义级流循环（窗口/阈值取自当前配置）。
fn reason_stream(cfg: &Config, stream: StreamName, kind: LoopKind) -> String {
    let (what, detail) = match kind {
        LoopKind::Character => {
            let window = match stream {
                StreamName::Thinking => cfg.int("thinkingWindow"),
                StreamName::Output => cfg.int("outputWindow"),
            };
            ("char loop", format!("repeated {window}-char block"))
        }
        LoopKind::Semantic => (
            "semantic loop",
            format!("{} repeated paragraphs", cfg.int("semanticThreshold")),
        ),
    };
    format!(
        "⛔ loop-detect: terminated — {} {what} ({detail})",
        stream.as_str()
    )
}

/// 终止理由：跨轮思考停滞。
fn reason_stagnation(window: usize, threshold: f64) -> String {
    let percent = (threshold * 100.0).round() as i64;
    format!(
        "⛔ loop-detect: terminated — reasoning stagnation across last {window} turns ({percent}%+ similar)"
    )
}

/// 终止理由：工具调用序列循环。
fn reason_tool_loop(name: &str, window_size: usize) -> String {
    format!(
        "⛔ loop-detect: terminated — tool call loop: same {window_size}-call sequence repeating ({name})"
    )
}

/// 词集 Jaccard 相似度（小写、按空白切词）。
fn jaccard(a: &str, b: &str) -> f64 {
    let set_a: HashSet<String> = a
        .to_lowercase()
        .split_whitespace()
        .map(str::to_string)
        .collect();
    let set_b: HashSet<String> = b
        .to_lowercase()
        .split_whitespace()
        .map(str::to_string)
        .collect();
    let inter = set_a.intersection(&set_b).count();
    let union = set_a.len() + set_b.len() - inter;
    if union == 0 {
        1.0
    } else {
        inter as f64 / union as f64
    }
}

/// 带最小词数门槛的相似度：任一侧词数低于 [`MIN_WORDS_FOR_SIMILARITY`] 时返回
/// `None`（不可比）。避免“Let me read the file.”这类短思考块之间虚高的相似度
/// 把停滞检测误伤。
fn similarity(a: &str, b: &str) -> Option<f64> {
    let words = |s: &str| s.split_whitespace().count();
    if words(a) < MIN_WORDS_FOR_SIMILARITY || words(b) < MIN_WORDS_FOR_SIMILARITY {
        return None;
    }
    Some(jaccard(a, b))
}

/// 判断工具是否在豁免名单内（逗号分隔，忽略大小写与首尾空白）。
fn is_exempt_tool(name: &str, exempt: &str) -> bool {
    if exempt.trim().is_empty() {
        return false;
    }
    exempt
        .split(',')
        .any(|t| t.trim().eq_ignore_ascii_case(name))
}

/// 工具调用指纹：`name:{stable json}`（serde_json 对象键默认有序）。
fn hash_tool_call(name: &str, args: &Value) -> String {
    format!("{name}:{}", serde_json::to_string(args).unwrap_or_default())
}

/// 取消息里最后一个 thinking 块文本。
fn extract_last_thinking(message: &Value) -> Option<String> {
    let content = message.get("content")?.as_array()?;
    for block in content.iter().rev() {
        if block.get("type").and_then(|v| v.as_str()) == Some("thinking")
            && let Some(t) = block.get("thinking").and_then(|v| v.as_str())
        {
            return Some(t.to_string());
        }
    }
    None
}

/// 检测历史尾部是否构成相邻重复序列（任意周期），返回周期长度。
fn detect_sequence_repeat(history: &[String]) -> usize {
    let n = history.len();
    for w in 1..=(n / 2) {
        let tail = &history[n - w..];
        let prev = &history[n - w * 2..n - w];
        if prev.len() == w && tail.iter().zip(prev).all(|(a, b)| a == b) {
            return w;
        }
    }
    0
}

/// Z 数组：`z[i]` = s 与 s[i..] 的最长公共前缀长度。
fn z_array(s: &[char]) -> Vec<usize> {
    let n = s.len();
    let mut z = vec![0usize; n];
    if n == 0 {
        return z;
    }
    z[0] = n;
    let (mut l, mut r) = (0usize, 0usize);
    for i in 1..n {
        if i < r {
            z[i] = (r - i).min(z[i - l]);
        }
        while i + z[i] < n && s[z[i]] == s[i + z[i]] {
            z[i] += 1;
        }
        if i + z[i] > r {
            l = i;
            r = i + z[i];
        }
    }
    z
}

/// 字符级循环：文本结尾是两段相邻的相同块（min..=max 字符）。
/// 只取末尾 2×max 字符做 Z 数组，返回循环起点前的干净前缀。
fn detect_repeating_suffix(
    text: &str,
    n: usize,
    min_window: usize,
    max_window: usize,
) -> Option<String> {
    let max_w = max_window.min(n / 2);
    if min_window == 0 || max_w < min_window {
        return None;
    }
    let tail: Vec<char> = text.chars().rev().take(2 * max_w).collect();
    let z = z_array(&tail);
    for (w, &zw) in z.iter().enumerate().take(max_w + 1).skip(min_window) {
        if zw >= w {
            return Some(text.chars().take(n - w).collect());
        }
    }
    None
}

/// 流式检测公共入口（thinking / output 共用）：按 `stream_stride(window)` 限频，
/// 先查字符级相邻重复，未命中再查段落级语义循环。
/// 返回 `(循环类型, 干净前缀)`；`window == 0` 跳过字符级，`threshold == 0` 跳过语义级。
fn check_stream(
    buf: &str,
    len: usize,
    last_checked_len: &mut usize,
    sem: &mut SemanticState,
    window: usize,
    semantic_threshold: usize,
) -> Option<(LoopKind, String)> {
    if window == 0 && semantic_threshold == 0 {
        return None;
    }
    let stride = stream_stride(window);
    if len < last_checked_len.saturating_add(stride) {
        return None;
    }
    *last_checked_len = len;

    if window > 0
        && let Some(clean) = detect_repeating_suffix(buf, len, window, max_window_for(window))
    {
        return Some((LoopKind::Character, clean));
    }
    if semantic_threshold > 0
        && let Some(clean) =
            detect_semantic_loop(buf, sem, PARA_MIN_LEN, FINGERPRINT_LEN, semantic_threshold)
    {
        return Some((LoopKind::Semantic, clean));
    }
    None
}

/// 段落语义循环：空行分隔的段落按前 `fingerprint_len` 字符指纹；
/// 同一指纹出现 `threshold` 次即触发。跳过 ``` 围栏内段落，并归一化有序列表序号。
fn detect_semantic_loop(
    text: &str,
    state: &mut SemanticState,
    para_min_len: usize,
    fingerprint_len: usize,
    threshold: usize,
) -> Option<String> {
    if threshold == 0 {
        return None;
    }

    let mut pos = state.scanned;
    let mut in_fence = state.in_fence;
    loop {
        let delim = find_blank_line(text, pos);
        let end = delim.map(|(start, _)| start).unwrap_or(text.len());
        let para = &text[pos..end];
        let fence_marks = para.matches("```").count();

        if !in_fence && fence_marks == 0 {
            let trimmed = para.trim();
            if char_len(trimmed) >= para_min_len {
                let fingerprint_text = strip_leading_counter(trimmed);
                let key: String = fingerprint_text.chars().take(fingerprint_len).collect();
                let count = state.counts.get(&key).copied().unwrap_or(0) + 1;
                if count >= threshold {
                    return Some(text[..pos].to_string());
                }
                if delim.is_some() {
                    state.counts.insert(key, count);
                }
            }
        }

        let (_, delim_end) = delim?;

        if fence_marks % 2 == 1 {
            in_fence = !in_fence;
        }

        pos = delim_end;
        state.scanned = pos;
        state.in_fence = in_fence;
    }
}

/// 找到 `from` 起第一个空行分隔段（`\r?\n[ \t]*\r?\n(?:[ \t]*\r?\n)*`）。
/// 返回 (分隔段起点, 分隔段终点)。
fn find_blank_line(text: &str, from: usize) -> Option<(usize, usize)> {
    let b = text.as_bytes();
    let mut i = from;
    while i < b.len() {
        if b[i] == b'\n' {
            let start = if i > from && b[i - 1] == b'\r' {
                i - 1
            } else {
                i
            };

            let mut j = skip_hspace(b, i + 1);
            if j < b.len() && b[j] == b'\r' && j + 1 < b.len() && b[j + 1] == b'\n' {
                j += 1;
            }

            if j < b.len() && b[j] == b'\n' {
                let mut end = j + 1;
                loop {
                    let mut k = skip_hspace(b, end);
                    if k < b.len() && b[k] == b'\r' {
                        k += 1;
                    }

                    if k < b.len() && b[k] == b'\n' {
                        end = k + 1;
                    } else {
                        break;
                    }
                }
                return Some((start, end));
            }
        }
        i += 1;
    }
    None
}

/// 从 `i` 起跳过空格与制表符，返回首个非空白字节的下标。
fn skip_hspace(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && (b[i] == b' ' || b[i] == b'\t') {
        i += 1;
    }
    i
}

/// 归一化有序列表序号：`23. foo` → `#. foo`（`#` 后保留 `.`/`)` 与空白）。
fn strip_leading_counter(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }

    if i > 0 && i < bytes.len() && (bytes[i] == b'.' || bytes[i] == b')') {
        let mut j = i + 1;
        if j < bytes.len() && (bytes[j] as char).is_whitespace() {
            while j < bytes.len() && (bytes[j] as char).is_whitespace() {
                j += 1;
            }

            let mut out = String::from("#");
            out.push(bytes[i] as char);
            out.push(' ');
            out.push_str(&s[j..]);
            return out;
        }
    }
    s.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::provider::ContentBlock;
    use crate::utils::time::civil_from_days;

    /// 重置检测状态与配置（配置在 reset() 里不清，避免会话内 set 被冲掉；测试需要洁净缺省）。
    fn fresh() {
        reset_all();
        lock_state().cfg = merge_config(None);
        // 注意：不在此清空全局 UI 队列（`take_pending_ui`）——该队列为进程级共享，
        // 并行测试（如 plan-mode）可能正在等待自己的请求；需要干净队列的用例
        // （stream_loop_queues_abort_and_notify）自行前后排空。
    }

    fn cfg_default() -> Config {
        merge_config(None)
    }

    #[test]
    fn jaccard_bounds() {
        assert_eq!(jaccard("a b c", "a b c"), 1.0);
        assert_eq!(jaccard("a b", "c d"), 0.0);
        assert_eq!(jaccard("", ""), 1.0);
        let mid = jaccard("a b c d", "a b x y");
        assert!((mid - 0.333).abs() < 0.01, "mid={mid}");
    }

    #[test]
    fn repeating_suffix_finds_adjacent_copy() {
        // 两个相邻的 80 字符块；max_w = n/2 = 80，min_window=80 命中，干净前缀为第一块
        let unit: String = "0123456789".repeat(8);
        let text = format!("{unit}{unit}");
        let n = char_len(&text);
        let clean = detect_repeating_suffix(&text, n, 80, 4000).expect("hit");
        assert_eq!(char_len(&clean), 80);
        // 无重复
        assert!(detect_repeating_suffix("hello world", 11, 80, 4000).is_none());
        // 文本不足 2×min_window → 不命中
        assert!(detect_repeating_suffix(&unit, 80, 80, 4000).is_none());
    }

    #[test]
    fn semantic_loop_counts_paragraph_repeats() {
        let para = "This is a reasonably long paragraph of reasoning text that repeats.";
        let text = format!("{para}\n\n{para}\n\n{para}\n\n");
        let mut state = SemanticState::new();
        let hit = detect_semantic_loop(&text, &mut state, PARA_MIN_LEN, FINGERPRINT_LEN, 3);
        assert!(hit.is_some(), "third repeat should fire");
    }

    #[test]
    fn semantic_loop_normalizes_list_counters() {
        let a = "23. Step through the repository and enumerate every module carefully.";
        let b = "28. Step through the repository and enumerate every module carefully.";
        let c = "33. Step through the repository and enumerate every module carefully.";
        let text = format!("{a}\n\n{b}\n\n{c}\n\n");
        let mut state = SemanticState::new();
        assert!(
            detect_semantic_loop(&text, &mut state, 40, 60, 3).is_some(),
            "changing ordinals must not disguise repeats"
        );
    }

    #[test]
    fn semantic_loop_skips_code_fences() {
        let para = "A long paragraph that lives inside a fenced code block and repeats.";
        let text = format!("```\n{para}\n{para}\n{para}\n```\n\nend");
        let mut state = SemanticState::new();
        assert!(detect_semantic_loop(&text, &mut state, 40, 60, 3).is_none());
    }

    #[test]
    fn sequence_repeat_detects_any_period() {
        let h = |items: &[&str]| items.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(detect_sequence_repeat(&h(&["a", "a"])), 1);
        assert_eq!(detect_sequence_repeat(&h(&["a", "b", "a", "b"])), 2);
        assert_eq!(detect_sequence_repeat(&h(&["a", "b", "c"])), 0);
        assert_eq!(
            detect_sequence_repeat(&h(&["a", "b", "c", "a", "b", "c"])),
            3
        );
    }

    #[test]
    fn config_validation_rejects_bad_values() {
        let file = json!({
            "thinkingWindow": -5,
            "stagnationThreshold": 1.5,
            "toolLoop": 9,
            "hookCmd": "node x.mjs",
            "toolLoopExempt": 42
        });
        let cfg = merge_config(Some(&file));
        assert_eq!(cfg.int("thinkingWindow"), 80, "negative falls back");
        assert_eq!(cfg.num("stagnationThreshold"), 0.85, ">1 falls back");
        assert_eq!(cfg.int("toolLoop"), 1, ">1 falls back");
        assert_eq!(cfg.text("hookCmd"), "node x.mjs");
        assert_eq!(cfg.text("toolLoopExempt"), "", "wrong type falls back");
    }

    #[test]
    fn command_parses_subcommand_after_command_name() {
        // 回归：命令分发传入完整命令文本（如 `loop-detect reset`），
        // 必须先剥掉命令名，否则 `reset` 永远不命中，落回状态输出。
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        fresh();
        let mut app = App::new();

        // reset 分支同样要命中（先置脏再整体复位）
        lock_state().tool_history.push("x".to_string());
        command_loop_detect(&mut app, "loop-detect reset");
        assert!(
            lock_state().tool_history.is_empty(),
            "`loop-detect reset` 应复位状态"
        );

        fresh();
    }

    #[test]
    fn loop_detect_subcommands_match_parser() {
        // 输入框面板提示的子命令必须都能被解析器执行（否则 Tab/Enter 补全出一个不生效的子命令）。
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        fresh();

        let declared = LoopDetect
            .commands()
            .into_iter()
            .find(|c| c.name == CMD)
            .expect("loop-detect 命令应注册")
            .subcommands;
        let names: Vec<&str> = declared.iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["reset"], "只声明解析器实现的子命令");

        for sub in &declared {
            let mut app = App::new();
            lock_state().tool_history.push("x".to_string());
            command_loop_detect(&mut app, &format!("{} {}", CMD, sub.name));
            assert!(
                lock_state().tool_history.is_empty(),
                "{} 应复位状态",
                sub.name
            );
        }

        fresh();
    }

    #[test]
    fn tool_exempt_list_is_case_insensitive() {
        assert!(is_exempt_tool("Bash", "bash,run_tests"));
        assert!(!is_exempt_tool("read", "bash"));
        assert!(!is_exempt_tool("read", "  "));
    }

    #[test]
    fn strip_leading_counter_normalizes() {
        assert_eq!(strip_leading_counter("23. foo"), "#. foo");
        assert_eq!(strip_leading_counter("7) bar"), "#) bar");
        assert_eq!(strip_leading_counter("plain"), "plain");
        assert_eq!(
            strip_leading_counter("23.foo"),
            "23.foo",
            "no whitespace → untouched"
        );
    }

    #[test]
    fn iso_timestamp_shape() {
        let ts = now_iso();
        assert!(ts.ends_with('Z') && ts.len() == 24, "ts={ts}");
        assert_eq!(civil_from_days(0), (1970, 1, 1));
    }

    #[test]
    fn tool_loop_detects_sequence_and_does_not_count_blocked() {
        // 与其它使用全局状态的测试串行（状态全局单例）
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        fresh();
        let args = json!({"command": "ls"});
        {
            let mut st = lock_state();
            // 第一次放行并记录
            assert!(check_tool_call(&mut st, "bash", &args).is_none());
            assert_eq!(st.tool_history.len(), 1);
            // 第二次相邻重复：命中，且不记录（历史保持 1）
            let hit = check_tool_call(&mut st, "bash", &args).expect("second identical call");
            assert_eq!(hit.window_size, 1);
            assert!(hit.reason.contains("tool call loop"));
            assert_eq!(st.tool_history.len(), 1, "blocked call not recorded");
            // 换成不同参数：相邻性被打断，再次放行
            let other = json!({"command": "pwd"});
            assert!(check_tool_call(&mut st, "bash", &other).is_none());
            assert_eq!(st.tool_history.len(), 2);
        }
        fresh();
    }

    #[test]
    fn tool_loop_off_disables_detector() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        fresh();
        {
            let mut st = lock_state();
            st.cfg.values.insert("toolLoop".into(), json!(0));
            let args = json!({"command": "ls"});
            assert!(check_tool_call(&mut st, "bash", &args).is_none());
            assert!(check_tool_call(&mut st, "bash", &args).is_none());
            assert!(st.tool_history.is_empty(), "关闭时不记录");
        }
        fresh();
    }

    #[test]
    fn transform_context_is_probe_only() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        fresh();
        let mut messages = vec![AgentMessage {
            role: "assistant".into(),
            thinking_level: None,
            content: vec![ContentBlock::Thinking {
                thinking: "loop reasoning".into(),
                thinking_signature: Some("sig".into()),
                redacted: None,
            }],
            tool_call_id: None,
            tool_name: None,
            is_error: false,
            stop_reason: None,
            error_message: None,
            model: None,
            provider: None,
            api: None,
            response_model: None,
            response_id: None,
            diagnostics: None,
            usage: None,
            deferred: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 0,
            duration_ms: None,
            details: None,
            citations: None,
            entry_id: None,
        }];
        LoopDetect.transform_context(&mut messages).unwrap();
        // 回归：本扩展不改写/检索历史；传入上下文必须一字不改
        assert_eq!(messages.len(), 1);
        match &messages[0].content[0] {
            ContentBlock::Thinking {
                thinking,
                thinking_signature,
                ..
            } => {
                assert_eq!(thinking, "loop reasoning");
                assert_eq!(thinking_signature.as_deref(), Some("sig"));
            }
            other => panic!("expected untouched thinking block, got {other:?}"),
        }
        // 但仍须完成 run 上电（Probe 角色）
        let st = lock_state();
        assert!(st.run_active, "transform_context 应完成上电");
        assert_eq!(st.turn_index, 1, "首个 turn_start 被错过时应补齐计数");
        drop(st);
        fresh();
    }

    #[test]
    fn detection_payload_shape() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        fresh();
        let payload = {
            let st = lock_state();
            build_payload(
                &st,
                "tool_loop",
                json!({"toolName": "bash", "windowSize": 2}),
            )
        };
        assert_eq!(payload["type"], "loop-detect:detection");
        assert_eq!(payload["event"], "tool_loop");
        assert_eq!(payload["details"]["toolName"], "bash");
        assert!(payload["timestamp"].as_str().unwrap().ends_with('Z'));
        assert!(payload.get("sessionId").is_some());
        assert!(payload.get("cwd").is_some());
        fresh();
    }

    #[test]
    fn stream_loop_queues_abort_and_notify_only() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        while extensions::take_pending_ui().is_some() {}
        fresh();
        {
            let mut st = lock_state();
            st.cfg.values.insert("thinkingWindow".into(), json!(20));
            st.cfg.values.insert("semanticThreshold".into(), json!(0));
        }
        let unit = "abcdefghijklmnopqrst"; // 20 chars
        let text = format!("{unit}{unit}");
        LoopDetect
            .on_agent_event(&json!({"type": "message_start", "message": {"role": "assistant"}}));
        LoopDetect
            .on_agent_event(&json!({"type": "message_update", "assistantMessageEvent": {"type": "thinking_start", "contentIndex": 0}}));
        let chars: Vec<char> = text.chars().collect();
        for chunk in chars.chunks(5) {
            let delta: String = chunk.iter().collect();
            LoopDetect.on_agent_event(&json!({
                "type": "message_update",
                "assistantMessageEvent": {"type": "thinking_delta", "contentIndex": 0, "delta": delta}
            }));
        }
        {
            let st = lock_state();
            assert!(st.stream_aborted, "detector should have fired");
            assert_eq!(st.loop_stream, StreamName::Thinking);
            assert_eq!(st.loop_kind, LoopKind::Character);
        }
        // 只应排队 AbortRun + Notify（系统消息）；绝不再投递归跑消息。
        let mut saw_abort = false;
        let mut saw_notify = false;
        while let Some(req) = extensions::take_pending_ui() {
            match req {
                ExtensionUiRequest::AbortRun => saw_abort = true,
                ExtensionUiRequest::Notify { text, .. } => {
                    saw_notify = true;
                    assert!(text.contains("terminated"), "理由应说明终止: {text}");
                }
                ExtensionUiRequest::Continuation { .. } => {
                    panic!("纯检测扩展不应再投递续跑消息")
                }
                _ => {}
            }
        }
        assert!(saw_abort, "AbortRun must be requested");
        assert!(saw_notify, "终止理由系统消息必须发送");
        fresh();
        while extensions::take_pending_ui().is_some() {}
    }

    #[test]
    fn stagnation_terminates_run() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        while extensions::take_pending_ui().is_some() {}
        fresh();
        {
            let mut st = lock_state();
            st.cfg.values.insert("stagnationWindow".into(), json!(2));
            st.cfg
                .values
                .insert("stagnationThreshold".into(), json!(0.5));
            st.cfg.values.insert("thinkingWindow".into(), json!(0));
            st.cfg.values.insert("semanticThreshold".into(), json!(0));
            st.cfg.values.insert("toolLoop".into(), json!(0));
        }
        let thinking = "we should keep reading the same file and then continue with the task";
        for _ in 0..2 {
            LoopDetect.on_agent_event(&json!({
                "type": "message_end",
                "message": {"role": "assistant", "content": [{"type": "thinking", "thinking": thinking}]}
            }));
        }
        let mut saw_abort = false;
        let mut saw_notify = false;
        while let Some(req) = extensions::take_pending_ui() {
            match req {
                ExtensionUiRequest::AbortRun => saw_abort = true,
                ExtensionUiRequest::Notify { text, .. } => {
                    saw_notify = true;
                    assert!(text.contains("stagnation"), "理由应说明停滞: {text}");
                }
                ExtensionUiRequest::Continuation { .. } => panic!("不应投递续跑消息"),
                _ => {}
            }
        }
        assert!(saw_abort && saw_notify, "停滞应终止 run 并发系统消息");
        fresh();
        while extensions::take_pending_ui().is_some() {}
    }

    /// 命中同时置位「回合后终止」：`turn_end` 边界返回 `end`（只消费一次），
    /// 与 `AbortRun` 互补——不依赖 UI 事件循环即可确定性收尾。
    #[test]
    fn detection_ends_run_at_turn_end_boundary() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        while extensions::take_pending_ui().is_some() {}
        fresh();
        {
            let mut st = lock_state();
            st.cfg.values.insert("stagnationWindow".into(), json!(2));
            st.cfg
                .values
                .insert("stagnationThreshold".into(), json!(0.5));
            st.cfg.values.insert("thinkingWindow".into(), json!(0));
            st.cfg.values.insert("semanticThreshold".into(), json!(0));
            st.cfg.values.insert("toolLoop".into(), json!(0));
        }

        // 未命中：边界不参与决策
        assert!(
            LoopDetect
                .on_boundary(&json!({ "type": "turn_end", "outcome": "completed" }))
                .is_none(),
            "未命中时不应干预边界"
        );

        let thinking = "we should keep reading the same file and then continue with the task";
        for _ in 0..2 {
            LoopDetect.on_agent_event(&json!({
                "type": "message_end",
                "message": {"role": "assistant", "content": [{"type": "thinking", "thinking": thinking}]}
            }));
        }
        let outcome = LoopDetect
            .on_boundary(&json!({ "type": "turn_end", "outcome": "completed" }))
            .expect("命中后 turn_end 边界应返回决策");
        assert!(outcome.end && !outcome.r#continue, "应要求优雅结束 run");

        // 只消费一次：同一 run 的后续回合不再重复 end
        assert!(
            LoopDetect
                .on_boundary(&json!({ "type": "turn_end", "outcome": "completed" }))
                .is_none(),
            "终止标志应一次性消费"
        );
        // 结算边界与本扩展无关
        assert!(
            LoopDetect
                .on_boundary(&json!({ "type": "agent_before_settle", "outcome": "completed" }))
                .is_none()
        );

        fresh();
        while extensions::take_pending_ui().is_some() {}
    }

    #[test]
    fn run_end_resets_all_detection_state() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        fresh();
        let dirty = |st: &mut State| {
            begin_run(st);
            st.stream_aborted = true;
            st.tool_history.push("bash:{}".to_string());
            st.thinking_history.push("stale thinking".to_string());
            st.turn_index = 3;
            st.loop_stream = StreamName::Output;
            st.loop_kind = LoopKind::Semantic;
        };
        {
            let mut st = lock_state();
            dirty(&mut st);
        }
        on_agent_end();
        {
            let st = lock_state();
            assert!(!st.run_active, "agent_end 应断电");
            assert!(!st.stream_aborted);
            assert!(st.tool_history.is_empty());
            assert!(st.thinking_history.is_empty());
            assert_eq!(st.turn_index, 0);
            assert_eq!(st.loop_stream, StreamName::Thinking);
            assert_eq!(st.loop_kind, LoopKind::Character);
        }
        // agent_settled 同样整段重置
        {
            let mut st = lock_state();
            dirty(&mut st);
        }
        on_agent_settled();
        {
            let st = lock_state();
            assert!(!st.run_active);
            assert!(st.tool_history.is_empty());
            assert_eq!(st.turn_index, 0);
        }
        fresh();
    }

    #[test]
    fn panel_choices_roundtrip() {
        for spec in panel_settings(&cfg_default()) {
            let mut cfg = cfg_default();
            for choice in &spec.choices {
                apply_panel_choice(&mut cfg, &spec.key, choice)
                    .unwrap_or_else(|e| panic!("apply {}/{choice}: {e}", spec.key));
                assert_eq!(
                    &panel_value(&cfg, &spec.key),
                    choice,
                    "roundtrip key={} choice={}",
                    spec.key,
                    choice
                );
            }
            assert!(
                apply_panel_choice(&mut cfg, &spec.key, "nope").is_err(),
                "{} 应拒绝非法档位",
                spec.key
            );
        }
    }

    #[test]
    fn settings_reflect_config_and_apply_persists() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        fresh();

        let settings = LoopDetect.settings();
        assert_eq!(settings.len(), panel_settings(&cfg_default()).len());
        let thinking = settings.iter().find(|s| s.key == "streamThinking").unwrap();
        assert_eq!(thinking.value, "normal");
        assert!(thinking.choices.contains(&"tight".to_string()));

        LoopDetect.apply_setting("streamThinking", "tight").unwrap();
        assert_eq!(lock_state().cfg.int("thinkingWindow"), 40);
        // 面板是设置语义：立即落盘到 extensions/ 子目录
        let path = crate::core::settings_manager::agent_dir()
            .join("extensions")
            .join(CONFIG_FILE);
        assert_eq!(CONFIG_FILE, "loop-detect.json");
        assert!(path.exists(), "面板变更应立即写盘: {}", path.display());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"thinkingWindow\""));
        assert!(
            !text.contains("THINKING_WINDOW"),
            "落盘键名必须全小写: {text}"
        );

        assert!(LoopDetect.apply_setting("nope", "x").is_err());

        fresh();
    }

    #[test]
    fn config_file_writes_lowercase_keys_only() {
        let file = json!({
            "thinkingWindow": 40,
            "semanticThreshold": 2,
            "hookCmd": "node hook.mjs",
        });
        let cfg = merge_config(Some(&file));
        assert_eq!(cfg.int("thinkingWindow"), 40);
        assert_eq!(cfg.num("semanticThreshold"), 2.0);
        assert_eq!(cfg.text("hookCmd"), "node hook.mjs");

        let path = std::env::temp_dir().join(format!(
            "loop-detect-lowercase-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        write_config(&path, &cfg);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"thinkingWindow\""));
        assert!(text.contains("\"hookCmd\""));
        for key in default_keys() {
            assert!(text.contains(&format!("\"{key}\"")), "缺键 {key}");
        }
        let _ = std::fs::remove_file(&path);
    }

    /// 缓存以 `(path, mtime, len)` 为有效键：外部（非本模块 API）改写/删除文件后，
    /// 下一次 `load_config` 必须看到新值，而不是永远返回缓存里的旧快照。
    #[test]
    fn config_cache_reflects_external_edits() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        let path = config_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();

        std::fs::write(&path, r#"{"thinkingWindow": 40}"#).unwrap();
        assert_eq!(load_config().int("thinkingWindow"), 40);

        // 外部改写（长度也不同）→ 缓存失效，读到新值
        std::fs::write(&path, r#"{"thinkingWindow": 160}"#).unwrap();
        assert_eq!(load_config().int("thinkingWindow"), 160);

        // 删除后回落缺省（仍会按「缺文件」回写一次）
        std::fs::remove_file(&path).unwrap();
        assert_eq!(load_config().int("thinkingWindow"), 80);
    }

    #[test]
    fn no_args_opens_panel() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        fresh();
        let mut app = App::new();
        command_loop_detect(&mut app, "loop-detect");
        assert!(app.ext_settings.visible, "无参数应打开面板");
        assert_eq!(app.ext_settings.ext, EXT);
        // 再次运行同一命令应隐藏
        command_loop_detect(&mut app, "loop-detect");
        assert!(!app.ext_settings.is_open(), "再次运行应关闭面板");
        fresh();
    }

    #[test]
    fn similarity_gates_short_text() {
        assert!(similarity("ok", "ok").is_none(), "短文本不可比");
        let a = "we should read the file and then continue with the current task";
        let b = "we should read the file and then continue with the current work";
        let s = similarity(a, b).expect("足够长可比");
        assert!(s > 0.7, "s={s}");
    }

    #[test]
    fn reason_texts_name_the_detector() {
        let cfg = cfg_default();
        let s = reason_stream(&cfg, StreamName::Thinking, LoopKind::Character);
        assert!(s.contains("thinking") && s.contains("80-char"), "{s}");
        let s = reason_stream(&cfg, StreamName::Output, LoopKind::Semantic);
        assert!(s.contains("output") && s.contains("3 repeated"), "{s}");
        let s = reason_stagnation(4, 0.85);
        assert!(s.contains("4 turns") && s.contains("85%"), "{s}");
        let s = reason_tool_loop("bash", 2);
        assert!(s.contains("2-call") && s.contains("bash"), "{s}");
    }

    #[test]
    fn hooks_follow_run_state() {
        // 兴趣谓词决定框架是否分发：空闲不订阅高频面，run 进行中才上电。
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        fresh();
        let ext = LoopDetect;

        // 空闲：仅有每轮一次的 TransformContext 上电探针（不订阅边界：结算边界在 run 结束后
        // 才投递，订阅会让内核为每次 prompt 序列化整份 transcript），无高频面
        let hooks = ext.hooks();
        assert!(hooks.contains(&ExtensionHook::TransformContext));
        assert!(
            !hooks.contains(&ExtensionHook::Boundary),
            "空闲不订阅边界（run 结束后不再需要 turn_end 收尾）"
        );
        assert!(!hooks.contains(&ExtensionHook::AgentEvent));
        assert!(!hooks.contains(&ExtensionHook::BeforeToolCall));

        // 上电（run 进行中）→ 订阅高频事件与工具前拦截
        {
            let mut st = lock_state();
            begin_run(&mut st);
        }
        let hooks = ext.hooks();
        assert!(hooks.contains(&ExtensionHook::AgentEvent));
        assert!(hooks.contains(&ExtensionHook::BeforeToolCall));
        assert!(
            hooks.contains(&ExtensionHook::Boundary),
            "run 内需 turn_end 收尾"
        );

        // run 结束（agent_end / agent_settled）→ 整段重置并断电
        on_agent_end();
        assert!(!ext.hooks().contains(&ExtensionHook::AgentEvent));
        assert!(!ext.hooks().contains(&ExtensionHook::BeforeToolCall));
        assert!(
            !ext.hooks().contains(&ExtensionHook::Boundary),
            "run 结束后不再订阅边界"
        );

        begin_run(&mut lock_state());
        assert!(ext.hooks().contains(&ExtensionHook::AgentEvent));
        on_agent_settled();
        assert!(!ext.hooks().contains(&ExtensionHook::AgentEvent));

        // 全部检测器关闭 → 完全空闲（连探针都不订阅）
        {
            let mut st = lock_state();
            for k in [
                "thinkingWindow",
                "outputWindow",
                "semanticThreshold",
                "stagnationWindow",
                "toolLoop",
            ] {
                st.cfg.values.insert(k.to_string(), json!(0));
            }
        }
        assert!(ext.hooks().is_empty(), "无检测器：完全跳过");

        fresh();
    }
}
