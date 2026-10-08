//! agent_loop：可插拔的 Agent 主循环扩展面。
//!
//! `Agent` 持有一个可替换的 `AgentLoop`（`Option<AgentLoop>`，默认 `None`）：
//! - `None` → 走本模块内置的 [`default_loop`]；
//! - `Some(...)` → 走外部替换实现（见 [`Agent::set_agent_loop`]）。
//!
//! 替换实现通过 `Agent` 上的一组 `pub(crate)` 能力方法（`emit` / `take_steering_batch` /
//! `execute_tool_calls` / `maybe_compact_with` / `record_step_attempt` /
//! `emit_abort_failure` / `emit_agent_end` …）与内核交互，字段保持封装。
//!
//! `AgentLoop` 用 `Fn`（不可变调用）而非 `FnMut`：loop 本身无内部可变状态，故类型层面无锁、可重入，
//! 多个实例（含并发 `spawn_sub_agent` 的子代理）可同时调用同一 `Arc`，互不互斥。
//!
//! # 契约（自定义 loop 必须遵守）
//! - 尊重能力方法不变式：v4 durable 记录、事件流（`agent_start`/`turn_start`/`message_*`/
//!   `tool_execution_*`/`turn_end`/`agent_end`）契约、steering 消费、协作式 abort。
//! - 每轮收尾必须与内核一致地派发 `finish_turn` 与 `turn_end` 扩展边界
//!   （[`core::extensions::dispatch_boundary`]；决策在 `turn_end` 之后生效）。
//! - **不要**在同一 `Agent` 上再次调用 `run_loop()`/`default_loop`（会自递归）；
//!   子代理递归通过子实例的 `agent_loop` 字段派发。

use crate::{
    core::{
        self,
        agent_session::{
            Agent, FinishTurnAction, NextTurnSnapshot, TurnContext, stream_event_to_json,
        },
        auth,
        cache_warmer::WarmRequest,
        compaction::CompactionReason,
        extensions::{BoundaryOutcome, ExtensionHook},
        provider::{
            AgentMessage, ContentBlock, PayloadHook, StreamEvent, StreamResult, failure_message,
            stream_chat,
        },
        session_manager, session_v4,
        virtual_models::{self, ModelRouteReason, ModelRouteRequest},
    },
    error::{Error, Result},
    utils::time::now_ms,
};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use std::{
    borrow::Cow,
    sync::{Arc, atomic::Ordering},
};

/// 可插拔的 Agent 主循环签名：借用 `&mut Agent`，返回驱动该次 run 的 future。
/// `Fn`（非 `FnMut`）+ `Send + Sync`：无内部可变状态、无锁、可重入，多实例可并发调用同一实现。
pub type AgentLoop = Arc<dyn Fn(&mut Agent) -> BoxFuture<'_, Result<String>> + Send + Sync>;

/// 单轮 turn 的退出信号。
#[derive(PartialEq)]
enum TurnOutcome {
    /// 正常收尾，由循环继续判断（has_more_tool_calls / pending / explicit_continuation）
    Continue,
    /// 提前终止整个 run（abort / stopReason=error|aborted / finishTurn → End）
    RunFinished,
}

/// 本轮工具执行结果（供 `finalize_turn` 与循环状态消费）。
struct TurnToolResults {
    /// 是否还需继续内层循环（length 截断或工具未 terminate）
    has_more: bool,
    /// 本轮 toolResult 消息（源序）
    tool_result_msgs: Vec<AgentMessage>,
    /// 本轮 toolResult 的 JSON（turn_end 的 toolResults 载荷）
    turn_tool_results: Vec<Value>,
}

/// 跨轮 run 状态：被 [`run_turn`] 与 [`run_default_loop`] 共享，避免散落一堆 &mut 参数。
struct RunLoopState {
    /// v4 durable runId
    run_id: String,
    /// 本次 run 已记录的 step 尝试序号，逐次递增供 v4 durable 记录使用
    step_attempt_count: u32,
    /// run 最终 outcome（completed/aborted/failed）
    run_outcome: &'static str,
    /// 本次 run 新增消息（prompt 批 + 各轮产出）
    new_messages: Vec<AgentMessage>,
    /// 最终回复文本（仅无工具调用的回合）
    final_text: String,
    /// 是否已完成过至少一轮（第 2 轮起才做轮首准备）
    last_completed_turn: bool,
    /// 上一轮 assistant 消息（跨轮供 prepareNextTurn / finishTurn 使用）
    last_message: Option<AgentMessage>,
    /// 上一轮工具结果
    last_tool_results: Vec<AgentMessage>,
    /// 待注入的 steering 消息
    pending: Vec<AgentMessage>,
    /// 是否还有更多工具调用需要继续内层循环
    has_more_tool_calls: bool,
    /// `finishTurn` 返回 Continue 时置位：无自然请求（工具结果/steering/follow-up）也要再发起一次provider 请求；一次后清除。
    explicit_continuation: bool,
    /// 当前轮序号（0 起）：`turn_start` / `turn_end` 边界字段
    turn_index: u32,
}

/// 内置默认循环入口。`Agent::run_loop` 在字段为 `None` 时调用它。
pub fn default_loop(agent: &mut Agent) -> BoxFuture<'_, Result<String>> {
    Box::pin(run_default_loop(agent))
}

/// 默认循环实现（原 `Agent::run_loop` 主体）。
///
/// 循环：LLM 调用 + 工具执行 + steering 注入。
/// 终止条件：无工具调用且无 steering；或 stopReason=error/aborted；或 finishTurn → End。
/// 无迭代硬上限（依赖 terminate + 队列自然结束）。
///
/// 本函数只负责 run 起始/收尾与循环调度（operation record、agent_end）；
/// 单轮处理被拆分到 [`run_turn`]（轮首准备 → 注入 → LLM 调用 → 工具执行 → 收尾），
/// 跨轮状态由 [`RunLoopState`] 承载。
async fn run_default_loop(agent: &mut Agent) -> Result<String> {
    // 请求前检查凭据：未配置时给出明确指引（而不是 401 裸报错）。虚拟模型自身没有凭据：改为在路由时校验物理模型的凭据。
    if !virtual_models::is_virtual_api(&agent.model.api) && agent.model.api_key.trim().is_empty() {
        let note = auth::federation_note(&agent.model.provider)
            .map(|n| format!(" {n}"))
            .unwrap_or_default();

        return Err(Error::msg(format!(
            "No API key configured for provider {}. Run /login to store one.{note}",
            agent.model.provider
        )));
    }

    agent.emit(json!({ "type": "agent_start" }));

    // user 消息事件（本次注入批次：单条或 all 批量多条；运行中 steer 由 pending 处理发事件）
    let batch = std::mem::take(&mut agent.last_user_batch);

    // v4 durable operation 记录（runId 一次 run 一个；结束时 operation_finished）
    let run_id = session_manager::new_record_id("run");

    if let Some(s) = agent.session.as_mut() {
        s.append_lane_record(&session_v4::LaneRecord::OperationStarted {
            base: session_v4::RecordBase {
                id: run_id.clone(),
                seq: 0,
                lane: String::new(),
                timestamp: now_ms() as i64,
            },
            source_leaf_id: None,
            intent: session_v4::OperationIntent::Run {
                original_prompt: batch.clone(),
                initial_messages: vec![],
                system_prompt_override: None,
            },
        });
    }

    // 跨轮状态（runLoop 开始时先 poll 一次 steering）
    let mut state = RunLoopState {
        run_id,
        step_attempt_count: 0,
        run_outcome: "completed",
        new_messages: Vec::new(),
        final_text: String::new(),
        last_completed_turn: false,
        last_message: None,
        last_tool_results: Vec::new(),
        pending: agent.take_steering_batch(),
        has_more_tool_calls: true,
        explicit_continuation: false,
        turn_index: 0,
    };

    state.has_more_tool_calls = true;
    agent.emit(json!({ "type": "turn_start", "turnIndex": state.turn_index }));

    // 本次 run 新增消息（prompt 批次）
    for um in &batch {
        let v = serde_json::to_value(um).unwrap();
        agent.emit(json!({ "type": "message_start", "message": v }));
        agent.emit(json!({ "type": "message_end", "message": v }));
        state.new_messages.push(um.clone());
    }

    let mut run_error: Option<Error> = None;
    while state.has_more_tool_calls || !state.pending.is_empty() || state.explicit_continuation {
        match run_turn(agent, &mut state).await {
            Ok(TurnOutcome::RunFinished) => break,
            Ok(TurnOutcome::Continue) => {}
            // 单轮致命错误：先把 operation 收尾落盘（outcome=failed），再向调用方传播。
            // 若不写 finish，磁盘会永久残留一个"打开"的 operation：
            // 多次崩溃后累积成多个僵尸 open op，恢复时无法唯一确定当前运行。
            Err(e) => {
                state.run_outcome = "failed";
                run_error = Some(e);
                break;
            }
        }
    }

    // run 结束 → operation_finished（outcome 按终止路径；错误路径 outcome=failed 且带 error 载荷）
    if let Some(s) = agent.session.as_mut() {
        s.append_lane_record(&session_v4::LaneRecord::OperationFinished {
            base: session_v4::RecordBase {
                id: session_manager::new_record_id("op-fin"),
                seq: 0,
                lane: String::new(),
                timestamp: now_ms() as i64,
            },
            run_id: state.run_id.clone(),
            outcome: state.run_outcome.to_string(),
            error: run_error
                .as_ref()
                .map(|e| json!({ "message": e.to_string() })),
        });
    }

    // 持久化写盘失败 → TUI error 事件（红色状态栏），不静默吞掉
    if let Some(s) = agent.session.as_mut()
        && let Some(err) = s.take_persist_error()
    {
        agent.emit(json!({ "type": "error", "error": err }));
    }

    agent.emit_agent_end(&state.new_messages);

    if let Some(e) = run_error {
        return Err(e);
    }

    Ok(state.final_text)
}

/// 轮首准备（第 2 轮起）：协作式取消检查、轮间阈值压缩、prepareNextTurn、steering 补充、turn_start。
/// 返回 false 表示检测到 abort，应结束整个 run。
async fn start_turn(agent: &mut Agent, st: &mut RunLoopState) -> Result<bool> {
    // 协作式取消：有 abort 请求时不再发起新 LLM 调用
    if agent.run_abort.load(Ordering::Relaxed) {
        st.run_outcome = "aborted";
        agent.emit_abort_failure(&mut st.new_messages);
        return Ok(false);
    }

    // 轮间阈值压缩（多轮 steering/follow-up 时每轮间检查）。
    // 虚拟选择跳过：窗口要按路由后的物理模型算，见 [`resolve_virtual_route`]。
    if agent.auto_compaction && !agent.selected_is_virtual() {
        _ = agent
            .maybe_compact_with(CompactionReason::Threshold, None)
            .await;
    }

    let snap = agent.prepare_next_turn.as_ref().and_then(|m| {
        let turn_ctx = TurnContext {
            message: st
                .last_message
                .clone()
                .unwrap_or_else(|| AgentMessage::user_text("")),
            tool_results: st.last_tool_results.clone(),
            context: agent.messages.clone(), // 轮后当前 agent 上下文;
            new_messages: st.new_messages.clone(), // 本循环返回的新消息（prompt 批次 + 本轮产出）
        };
        m.lock().ok().and_then(|mut g| {
            let f: &mut dyn FnMut(&TurnContext) -> Option<NextTurnSnapshot> = g.as_mut();
            f(&turn_ctx)
        })
    });

    if let Some(snap) = snap {
        if let Some(ctx) = snap.context {
            agent.messages = ctx;
        }
        if let Some(m) = snap.model {
            agent.model = m;
        }
        if let Some(lvl) = snap.thinking_level {
            agent.set_thinking_level(&lvl);
        }
    }

    if st.pending.is_empty() {
        st.pending = agent.take_steering_batch();
    }
    st.turn_index += 1;
    agent.emit(json!({ "type": "turn_start", "turnIndex": st.turn_index }));
    Ok(true)
}

/// 处理一次 LLM 轮次。返回 [`TurnOutcome`] 表达是否提前终止 run。
async fn run_turn(agent: &mut Agent, st: &mut RunLoopState) -> Result<TurnOutcome> {
    // finishTurn → Continue 是一次性延续：本轮已进入请求流程，标志即可清除
    st.explicit_continuation = false;

    // 轮首准备（第 2 轮起）：协作式取消检查 + 轮间压缩 + prepareNextTurn + steering 补充 + turn_start
    if st.last_completed_turn && !start_turn(agent, st).await? {
        return Ok(TurnOutcome::RunFinished);
    }

    // 注入 steering 消息（turn 开始前 push + 事件）
    inject_pending(agent, st);

    // 虚拟模型路由（每请求一次）：选中虚拟模型时解析物理模型 + thinking 级别。
    agent.clear_routed_model();
    if agent.selected_is_virtual() && !resolve_virtual_route(agent, st).await? {
        return Ok(TurnOutcome::RunFinished);
    }

    // OAuth 凭据刷新 + TransformContext 钩子
    prepare_model_context(agent).await?;

    // LLM 调用 + on_response 通知
    let result = call_model(agent).await?;
    let mut assistant_msg = result.message;

    // 记录本轮响应所用的 thinking 级别：级别会在会话中途变化，逐消息记录才能反映每条响应的真实档位。
    // 虚拟模型记录的是**路由后**物理模型的级别。
    assistant_msg.thinking_level = Some(
        agent
            .effective_thinking_level()
            .unwrap_or_else(|| "off".to_string()),
    );
    let stop_reason = assistant_msg.stop_reason.clone();

    // stopReason=error/aborted → 立即终止（消息进流，不抛错）
    if is_terminal_stop_reason(&stop_reason) {
        st.run_outcome = terminal_outcome(&stop_reason);
        finish_error_turn(agent, st, &mut assistant_msg);
        return Ok(TurnOutcome::RunFinished);
    }

    // 提取工具调用 + assistant 落库（工具前，tool_started 需要 assistantEntryId）
    let tool_calls = collect_tool_calls(&assistant_msg);
    record_assistant(agent, st, &mut assistant_msg);

    // 工具执行调度（length 截断 / 正常 / 无工具）
    let turn_tools = execute_turn_tools(agent, st, &tool_calls, &stop_reason, &assistant_msg).await;
    st.has_more_tool_calls = turn_tools.has_more;

    // 本轮工具执行中激活了延迟工具（tool_search）：重建工具表，下一轮模型调用即可见
    if core::extensions::take_deferred_activation_dirty() {
        agent.rebuild_tools();
    }

    // 收尾：finishTurn → turn_end 事件、消息推送、final_text、steering poll
    Ok(finalize_turn(
        agent,
        st,
        &assistant_msg,
        !tool_calls.is_empty(),
        &stop_reason,
        turn_tools,
    ))
}

/// 注入 steering 消息（turn 开始前 push + 事件）。
fn inject_pending(agent: &mut Agent, st: &mut RunLoopState) {
    if st.pending.is_empty() {
        return;
    }
    let msgs = std::mem::take(&mut st.pending);
    for mut m in msgs {
        if let Some(s) = agent.session.as_mut() {
            let entry_id = s.append_message(&m);
            m.entry_id = Some(entry_id);
        }
        let v = serde_json::to_value(&m).unwrap();
        agent.emit(json!({ "type": "message_start", "message": v }));
        agent.emit(json!({ "type": "message_end", "message": v }));
        st.new_messages.push(m.clone());
        agent.messages.push(m);
    }
}

/// 每轮 LLM 调用前的上下文准备：OAuth 凭据过期刷新 + TransformContext 钩子。
async fn prepare_model_context(agent: &mut Agent) -> Result<()> {
    // OAuth 凭据过期自动刷新（按本次请求**实际使用**的模型所属 provider）
    let provider = agent.effective_model().provider.clone();
    if let Some(access) = auth::ensure_oauth_valid(&provider).await? {
        match agent.routed_model.as_mut() {
            Some(routed) => {
                if routed.api_key != access {
                    routed.api_key = access;
                }
            }
            None => {
                if agent.model.api_key != access {
                    agent.model.api_key = access;
                }
            }
        }
    }

    // TransformContext 钩子：每轮 LLM 调用前
    for ext in core::extensions::registered() {
        if ext.hooks().contains(&ExtensionHook::TransformContext) {
            ext.transform_context(&mut agent.messages)?;
        }
    }
    Ok(())
}

/// 本轮 `messages` 里最后一条 assistant 之后是否有 user 消息（判定 `user` 理由）。
fn route_user_turn(agent: &Agent) -> bool {
    let last_response = agent.messages.iter().rposition(|m| m.role == "assistant");
    agent.messages[last_response.map(|i| i + 1).unwrap_or(0)..]
        .iter()
        .any(|m| m.role == "user")
}

/// 虚拟模型路由：把本轮选中的虚拟模型解析为物理模型 + thinking 级别（每请求一次）。
///
/// 成功时写 `routed_model` / `routed_thinking_level`，必要时把路由器状态落到会话分支
/// （custom 条目）并广播 `model_routed`；随后按路由后的物理模型窗口做一次阈值压缩
/// （虚拟选择已在调用层 / 轮间跳过压缩）。
///
/// 路由失败时产出 `stopReason=error` 的助手消息并终止本次 run，返回 `Ok(false)`。
async fn resolve_virtual_route(agent: &mut Agent, st: &mut RunLoopState) -> Result<bool> {
    let provider = agent.model.provider.clone();
    let id = agent.model.model_id.clone();
    let previous = virtual_models::previous_route(&agent.messages);
    let failed = agent.take_pending_failed_route();

    let reason = if failed.is_some() {
        ModelRouteReason::Retry
    } else if route_user_turn(agent) {
        ModelRouteReason::User
    } else {
        ModelRouteReason::Continuation
    };

    let state = agent
        .session
        .as_ref()
        .and_then(|s| virtual_models::read_state(s, &provider, &id));
    let session_id = agent.session.as_ref().map(|s| s.session_id.clone());

    let request = ModelRouteRequest {
        selected: &agent.model,
        thinking_level: agent.thinking_level.as_deref(),
        reason,
        previous,
        failed,
        state: state.as_ref(),
        messages: &agent.messages,
        abort: Some(agent.run_abort.clone()),
    };
    let resolved =
        virtual_models::resolve_route(request, agent.api_key_override.clone(), session_id).await;

    let route = match resolved {
        Ok(route) => route,
        Err(e) => {
            let selected = agent.model.clone();
            let mut failure = failure_message(&selected, &e);
            agent.emit(json!({ "type": "message_start", "message": serde_json::to_value(&failure).unwrap() }));
            st.run_outcome = "failed";
            finish_error_turn(agent, st, &mut failure);
            return Ok(false);
        }
    };

    // 路由器返回了新状态（且与当前不同）→ 落到会话分支（供后续请求作为 `state` 传回）
    if let Some(new_state) = route.state.clone()
        && state.as_ref() != Some(&new_state)
        && let Some(session) = agent.session.as_mut()
    {
        virtual_models::append_state(session, &provider, &id, &new_state);
    }

    agent.routed_model = Some(route.model.clone());
    agent.routed_thinking_level = route.thinking_level.clone();
    agent.emit(json!({
        "type": "model_routed",
        "provider": route.model.provider,
        "model": route.model.model_id,
        "thinkingLevel": route.thinking_level,
    }));

    if agent.auto_compaction {
        _ = agent
            .maybe_compact_with(CompactionReason::Threshold, None)
            .await;
    }
    Ok(true)
}

/// 把 `(system_prompt, messages)` 组装成完整 transcript 交给扩展，取回**原样发送**的 `(messages, system_prompt)`。
///
/// 只影响本次请求，不改写会话历史（`agent.messages` / `agent.system_prompt` 不动）。
/// 无扩展声明该 hook 时返回 `None`（调用方直接用 agent 自己的上下文，零开销、不克隆）。
fn apply_context_with_system(agent: &Agent) -> Result<Option<(Vec<AgentMessage>, String)>> {
    if !core::extensions::has_context_with_system_handlers() {
        return Ok(None);
    }

    let has_system = !agent.system_prompt.trim().is_empty();
    let mut full: Vec<AgentMessage> = Vec::with_capacity(agent.messages.len() + 1);
    if has_system {
        let mut sys = AgentMessage::user_text(&agent.system_prompt);
        sys.role = "system".to_string();
        full.push(sys);
    }
    full.extend(agent.messages.iter().cloned());

    core::extensions::dispatch_context_with_system(&mut full)?;

    // 取回：首位仍是 system → 作为系统提示词；否则请求无系统提示词（处理器拥有该决定）
    if full.first().is_some_and(|m| m.role == "system") {
        let system_prompt = full[0].text();
        full.remove(0);
        Ok(Some((full, system_prompt)))
    } else {
        Ok(Some((full, String::new())))
    }
}

/// 发起 LLM 调用，更新 usage 并触发 on_response 通知。
async fn call_model(agent: &mut Agent) -> Result<StreamResult> {
    let result = perform_llm_call(agent).await?;
    agent.last_usage = result.usage.clone();
    if let Some(cb) = agent.on_response.as_ref()
        && let Ok(mut g) = cb.lock()
    {
        let f: &mut dyn FnMut(&Value) = g.as_mut();
        let model = agent.effective_model();
        f(&json!({
            "api": model.api,
            "provider": model.provider,
            "model": model.model_id,
            "usage": serde_json::to_value(&result.usage).unwrap_or(Value::Null),
            "stopReason": result.message.stop_reason,
            "role": result.message.role,
            "text": result.message.text(),
        }));
    }
    Ok(result)
}

/// stopReason 是否为立即终止类型（error / aborted）。
fn is_terminal_stop_reason(stop: &Option<String>) -> bool {
    matches!(stop.as_deref(), Some("error") | Some("aborted"))
}

/// 终止类型对应的 run outcome。
fn terminal_outcome(stop: &Option<String>) -> &'static str {
    if stop.as_deref() == Some("aborted") {
        "aborted"
    } else {
        "failed"
    }
}

/// stopReason=error/aborted 的收尾：记录 step_attempt、发事件、落库、置 final_text。
/// 结束后由调用方结束整个 run（agent_end 由 run_default_loop 尾统一发出）。
/// `finishTurn` 对 error/aborted 也会调用（在 `turn_end` 之前），但硬退出，其决策被忽略。
fn finish_error_turn(agent: &mut Agent, st: &mut RunLoopState, assistant_msg: &mut AgentMessage) {
    agent.record_step_attempt(
        &st.run_id,
        &mut st.step_attempt_count,
        assistant_msg.entry_id.as_deref(),
    );
    agent.emit(
        json!({ "type": "message_end", "message": serde_json::to_value(&*assistant_msg).unwrap() }),
    );

    // 先落库拿到 entryId：边界事件需要 messageEntryId
    if let Some(s) = agent.session.as_mut() {
        let entry_id = s.append_message(assistant_msg);
        assistant_msg.entry_id = Some(entry_id);
    }

    // finishTurn（硬退出：决策忽略）：上下文与正常收尾一致（含本轮 assistant）
    let turn_ctx = turn_context_with(agent, st, assistant_msg, &[]);
    _ = finish_turn_decision(agent, &turn_ctx);

    // turn_end 边界（硬退出：决策忽略）
    let turn_end_event = turn_end_event_json(
        agent,
        st,
        assistant_msg,
        &[],
        outcome_label(&assistant_msg.stop_reason),
    );

    let boundary = core::extensions::dispatch_boundary(&turn_end_event);
    agent.emit(turn_end_event);
    agent.apply_boundary_drafts(&boundary);

    st.new_messages.push(assistant_msg.clone());
    agent.messages.push(assistant_msg.clone());
    st.final_text = agent.messages.last().map(|m| m.text()).unwrap_or_default();
}

/// 调用 `finishTurn` 回调（`turn_end` 之前）。决策由调用方在 `turn_end` 之后应用。
fn finish_turn_decision(agent: &mut Agent, ctx: &TurnContext) -> Option<FinishTurnAction> {
    agent.finish_turn.as_ref().and_then(|m| {
        m.lock().ok().and_then(|mut g| {
            let f: &mut dyn FnMut(&TurnContext) -> Option<FinishTurnAction> = g.as_mut();
            f(ctx)
        })
    })
}

/// 合并 `finishTurn` 决策与扩展边界决策（`end` 优先，否则任一 `continue` 即续跑）。
fn merge_finish_decision(
    finish: Option<FinishTurnAction>,
    boundary: BoundaryOutcome,
) -> Option<FinishTurnAction> {
    let end = matches!(finish, Some(FinishTurnAction::End)) || boundary.end;
    let cont = matches!(finish, Some(FinishTurnAction::Continue)) || boundary.r#continue;
    if end {
        Some(FinishTurnAction::End)
    } else if cont {
        Some(FinishTurnAction::Continue)
    } else {
        None
    }
}

/// 轮结束时的活动结局
fn outcome_label(stop_reason: &Option<String>) -> &'static str {
    match stop_reason.as_deref() {
        Some("aborted") => "aborted",
        Some("error") => "error",
        _ => "completed",
    }
}

/// 构造 `turn_end` 事件（含边界字段，`TurnEndEvent`）。
fn turn_end_event_json(
    agent: &Agent,
    st: &RunLoopState,
    assistant_msg: &AgentMessage,
    tool_results: &[Value],
    outcome: &str,
) -> Value {
    let tool_result_entry_ids: Vec<&str> = st
        .last_tool_results
        .iter()
        .filter_map(|m| m.entry_id.as_deref())
        .collect();
    json!({
        "type": "turn_end",
        "turnIndex": st.turn_index,
        "message": serde_json::to_value(assistant_msg).unwrap(),
        "toolResults": tool_results,
        "messageEntryId": assistant_msg.entry_id,
        "toolResultEntryIds": tool_result_entry_ids,
        "outcome": outcome,
        // 边界事件同款字段：扩展区分会话主 agent 与子代理 run
        "agentScope": if agent.is_subagent { "subagent" } else { "main" },
    })
}

/// 构造收尾回调入参（消息尚未推入 `agent.messages` / `new_messages` 时使用）。
fn turn_context_with(
    agent: &Agent,
    st: &RunLoopState,
    assistant_msg: &AgentMessage,
    tool_results: &[AgentMessage],
) -> TurnContext {
    let mut context = agent.messages.clone();
    context.push(assistant_msg.clone());
    context.extend(tool_results.iter().cloned());

    let mut new_messages = st.new_messages.clone();
    new_messages.push(assistant_msg.clone());
    new_messages.extend(tool_results.iter().cloned());

    TurnContext {
        message: assistant_msg.clone(),
        tool_results: tool_results.to_vec(),
        context,
        new_messages,
    }
}

/// 从 assistant 消息提取工具调用块。
fn collect_tool_calls(msg: &AgentMessage) -> Vec<ContentBlock> {
    msg.content
        .iter()
        .filter(|b| matches!(b, ContentBlock::ToolCall { .. }))
        .cloned()
        .collect()
}

/// assistant 结果落库（工具执行前）+ step_attempt 记录。
/// 先发 message_end，再 append_message 获取 entryId，最后 record_step_attempt。
fn record_assistant(agent: &mut Agent, st: &mut RunLoopState, assistant_msg: &mut AgentMessage) {
    // assistant message_end 在工具执行前发出
    agent.emit(
        json!({ "type": "message_end", "message": serde_json::to_value(&*assistant_msg).unwrap() }),
    );

    // assistant 结果先落库（工具前），tool_started 记录需要 assistantEntryId
    if let Some(s) = agent.session.as_mut() {
        let entry_id = s.append_message(assistant_msg);
        assistant_msg.entry_id = Some(entry_id);
    }
    agent.record_step_attempt(
        &st.run_id,
        &mut st.step_attempt_count,
        assistant_msg.entry_id.as_deref(),
    );
}

/// 工具执行调度：
/// - length 且含工具调用 → 不执行，生成错误 toolResult（terminate=false → 继续）；
/// - 正常工具调用 → `execute_tool_calls`（beforeToolCall 入参 assistantMessage + 当前上下文）；
/// - 无工具调用 → 无操作。
async fn execute_turn_tools(
    agent: &mut Agent,
    st: &mut RunLoopState,
    tool_calls: &[ContentBlock],
    stop_reason: &Option<String>,
    assistant_msg: &AgentMessage,
) -> TurnToolResults {
    let mut out = TurnToolResults {
        has_more: false,
        tool_result_msgs: Vec::new(),
        turn_tool_results: Vec::new(),
    };

    if stop_reason.as_deref() == Some("length") && !tool_calls.is_empty() {
        // 不执行工具，生成错误 toolResult
        emit_truncated_tool_results(agent, st, tool_calls, &mut out.turn_tool_results);
        out.has_more = true; // terminate=false → 继续
    } else if !tool_calls.is_empty() {
        // 正常工具调用流程
        let context = agent.messages.clone();
        let (results, terminate) = agent
            .execute_tool_calls(
                tool_calls,
                &st.run_id,
                assistant_msg.entry_id.as_deref(),
                assistant_msg,
                &context,
            )
            .await;

        for (_, rm) in &results {
            out.tool_result_msgs.push(rm.clone());
            out.turn_tool_results
                .push(serde_json::to_value(rm).unwrap());
        }
        out.has_more = !terminate;
    }
    out
}

/// 本轮收尾：消息推送、final_text、last_completed_turn、`finishTurn` 回调、
/// `turn_end` 事件、决策应用、steering poll。返回是否终止 run。
///
/// `finishTurn` 在 assistant 与全部工具结果 finalize 之后、`turn_end` **之前** 调用，
/// 但其决策在 `turn_end` **之后**才生效；`End` 时跳过 prepareNextTurn 与 steering 轮询，且不消耗 steering 队列。
fn finalize_turn(
    agent: &mut Agent,
    st: &mut RunLoopState,
    assistant_msg: &AgentMessage,
    has_tool_calls: bool,
    stop_reason: &Option<String>,
    turn_tools: TurnToolResults,
) -> TurnOutcome {
    // 追加 assistant（含 tool_calls）+ toolResult 消息
    // （message_start/message_end 已分别由流 Start 事件与工具执行前的 emit 发出）
    let assistant_text = assistant_msg.text();

    // 记录本轮上下文（供 prepareNextTurn / finishTurn 下一轮使用）
    st.last_message = Some(assistant_msg.clone());
    st.last_tool_results = turn_tools.tool_result_msgs.clone();

    st.new_messages.push(assistant_msg.clone());
    agent.messages.push(assistant_msg.clone());

    for rm in turn_tools.tool_result_msgs {
        st.new_messages.push(rm.clone());
        agent.messages.push(rm);
    }

    // 仅无工具调用的回合可作为最终回复文本（工具轮 toolResult 不作为回复）。
    // 多轮（steering）时最后一条无工具回合文本胜出。
    if !has_tool_calls && stop_reason.as_deref() != Some("length") && !assistant_text.is_empty() {
        st.final_text = assistant_text;
    }

    st.last_completed_turn = true;

    // 每轮收尾回调（入参本轮上下文）；决策在 turn_end 之后应用
    let turn_ctx = TurnContext {
        message: st
            .last_message
            .clone()
            .unwrap_or_else(|| AgentMessage::user_text("")),
        tool_results: st.last_tool_results.clone(),
        context: agent.messages.clone(),
        new_messages: st.new_messages.clone(),
    };
    let finish_decision = finish_turn_decision(agent, &turn_ctx);

    // turn_end 边界：与 finishTurn 同一时机投给扩展（决策同样在 turn_end 之后生效）
    let turn_end_event = turn_end_event_json(
        agent,
        st,
        assistant_msg,
        &turn_tools.turn_tool_results,
        outcome_label(stop_reason),
    );
    let boundary = core::extensions::dispatch_boundary(&turn_end_event);

    agent.emit(turn_end_event);

    // 边界追加的投影草稿（context edit / retain-none 压缩）先落库生效，再应用决策
    agent.apply_boundary_drafts(&boundary);

    match merge_finish_decision(finish_decision, boundary) {
        // 结束 run：不 poll steering（队列保持原样），也不进下一轮 prepareNextTurn
        Some(FinishTurnAction::End) => {
            return TurnOutcome::RunFinished; // agent_end 由 run_default_loop 尾统一发出
        }
        // 保证一次后续 provider 请求：工具结果/steering 已能满足时不额外新增
        Some(FinishTurnAction::Continue) if !st.has_more_tool_calls && st.pending.is_empty() => {
            st.explicit_continuation = true;
        }
        Some(FinishTurnAction::Continue) => {}
        None => {}
    }

    // turn_end 后 poll steering
    st.pending = agent.take_steering_batch();
    TurnOutcome::Continue
}

/// stopReason=length 且含工具调用：不执行工具，为每个工具生成错误 toolResult 并发事件。
fn emit_truncated_tool_results(
    agent: &mut Agent,
    st: &mut RunLoopState,
    tool_calls: &[ContentBlock],
    turn_tool_results: &mut Vec<Value>,
) {
    for tc in tool_calls {
        let (id, name, arguments) = match tc {
            ContentBlock::ToolCall {
                id,
                name,
                arguments,
                ..
            } => (id.clone(), name.clone(), arguments.clone()),
            _ => continue,
        };
        let args_value = serde_json::to_value(&arguments).unwrap_or(Value::Null);

        agent.emit(json!({
            "type": "tool_execution_start",
            "toolCallId": id.clone(),
            "toolName": name.clone(),
            "args": args_value
        }));

        let err_text = format!(
            "Tool call \"{}\" was not executed: the response hit the output token limit, so its arguments may be truncated. Re-issue the tool call with complete arguments.",
            name
        );

        agent.emit(json!({
            "type": "tool_execution_end",
            "toolCallId": id.clone(),
            "toolName": name.clone(),
            "result": {
                "content": [ { "type": "text", "text": err_text } ],
                "details": {}
            },
            "isError": true
        }));

        let result_msg = AgentMessage {
            role: "toolResult".to_string(),
            thinking_level: None,
            content: vec![ContentBlock::Text {
                text: err_text,
                text_signature: None,
            }],
            tool_call_id: Some(id.clone()),
            tool_name: Some(name.clone()),
            is_error: true,
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
            timestamp: now_ms(),
            duration_ms: Some(0),
            details: None,
            citations: None,
            entry_id: None,
        };

        let result_json = serde_json::to_value(&result_msg).unwrap();
        turn_tool_results.push(result_json);

        let v = serde_json::to_value(&result_msg).unwrap();
        agent.emit(json!({ "type": "message_start", "message": v.clone() }));
        agent.emit(json!({ "type": "message_end", "message": v }));
        st.new_messages.push(result_msg.clone());

        if let Some(s) = agent.session.as_mut() {
            let eid = s.append_message(&result_msg);
            let mut rm = result_msg;
            rm.entry_id = Some(eid);
            agent.messages.push(rm);
        } else {
            agent.messages.push(result_msg);
        }
    }
}

/// 单次 LLM 调用（流式）：on_payload 钩子 + json_sink 事件收集 + provider 调用。
async fn perform_llm_call(agent: &mut Agent) -> Result<StreamResult> {
    let on_payload_hook: Option<PayloadHook> = agent.on_payload.clone();
    let mut collector = |ev: StreamEvent| {
        if let Some(sink) = agent.json_sink.as_ref()
            && let Ok(mut g) = sink.lock()
        {
            let f: &mut dyn FnMut(Value) = g.as_mut();
            let message = ev
                .partial()
                .map(|p| serde_json::to_value(p).unwrap_or(Value::Null))
                .unwrap_or(Value::Null);
            let is_start = matches!(&ev, StreamEvent::Start { .. });

            if is_start {
                // 流开始时即发 message_start（带初始快照）
                f(json!({ "type": "message_start", "message": message }));
            } else {
                let partial = ev.partial().cloned();
                let e = stream_event_to_json(ev, partial.as_ref());
                f(json!({
                    "type": "message_update",
                    "assistantMessageEvent": e,
                    "usage": null,
                }));
            }
        }
    };

    // `context_with_system`：结果原样发送（仅本次请求）；无处理器时不克隆上下文
    let applied = apply_context_with_system(agent)?;

    // 虚拟模型已路由到物理模型：请求、采样参数与 thinking 级别都用本次请求的有效值。
    let request_model = agent.effective_model().clone();
    let request_level = agent.effective_thinking_level();

    // `prepare_loadout().hidden_declarations`：这些工具仍激活、仍可被脚本/嵌套调，
    // 但本次请求不发给模型（`codemode.mode = only`）。无隐藏项时不克隆。
    let visible_tools: Cow<'_, [(String, String, Value)]> = if agent.hidden_declarations.is_empty()
    {
        Cow::Borrowed(&agent.tools)
    } else {
        Cow::Owned(
            agent
                .tools
                .iter()
                .filter(|t| !agent.hidden_declarations.contains(&t.0))
                .cloned()
                .collect(),
        )
    };

    let result = {
        let (messages, system_prompt): (&[AgentMessage], &str) = match &applied {
            Some((m, s)) => (m.as_slice(), s.as_str()),
            None => (agent.messages.as_slice(), agent.system_prompt.as_str()),
        };

        stream_chat(
            &request_model,
            messages,
            system_prompt,
            &visible_tools,
            request_level.as_deref(),
            request_model.temperature,
            None,
            Some(&mut collector),
            on_payload_hook,
            None,
        )
        .await
    };

    // 提示缓存保温：真实请求成功写出缓存后，按 TTL 后台重放同请求续命。仅会话主 agent、非离线、非失败/中止响应。
    let warmable = !agent.is_subagent
        && agent.session.is_some()
        && !agent.offline
        && !matches!(
            result.message.stop_reason.as_deref(),
            Some("error") | Some("aborted")
        );

    if warmable {
        let temperature = request_model.temperature;
        let (messages, system_prompt) = match applied {
            Some((m, s)) => (m, s),
            None => (agent.messages.clone(), agent.system_prompt.clone()),
        };

        _ = agent.cache_warmer.start(WarmRequest {
            model: request_model,
            messages,
            system_prompt,
            tools: visible_tools.into_owned(),
            reasoning_effort: request_level,
            temperature,
        });
    }

    Ok(result)
}
