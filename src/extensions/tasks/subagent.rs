//! 与 [`super::super::subagent`] 扩展的联动
//!
//! 经核心扩展事件总线做 scoped request/reply RPC。subagent 扩展已实现协议 v2 的全部端点：
//!
//! | 方向 | 通道 | 说明 |
//! |---|---|---|
//! | 发 | `subagents:rpc:ping` | 探活 + 版本校验，回 `<chan>:reply:<id>` `{success,data:{version}}` |
//! | 收 | `subagents:ready` | 对方初始化完成，重新 ping |
//! | 发 | `subagents:rpc:spawn` | `{requestId,type,prompt,options}` → `{id}` |
//! | 发 | `subagents:rpc:stop` | `{requestId,agentId}` |
//! | 发 | `subagents:rpc:consume` | fire-and-forget，抑制完成通知 |
//! | 收 | `subagents:completed` / `subagents:failed` | 生命周期终态 |
//!
//! 总线是同步广播、回执是异步事件，因此这里用 `oneshot` 把「发请求 → 等回执事件」桥成
//! 可 await 的调用。**绝不在 await 点持状态锁**。

use super::{
    EXT, State, lock_state,
    store::TaskStore,
    types::{Task, TaskStatusOrDeleted, TaskUpdateFields},
};
use crate::{
    core::{
        extensions::{
            self, ExtensionUiRequest, RichSpan, UiNotifyLevel, events as bus, request_ui,
        },
        tools::{ToolError, ToolResult},
    },
    extensions::{tasks::types::TaskStatus, util::make_id},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};
use tokio::sync::oneshot::{self, Sender};

/// RPC 协议版本（与 subagent 扩展对齐）。
pub const PROTOCOL_VERSION: u64 = 2;

/// 210、spawn 回执超时 30s；stop 10s。
const SPAWN_TIMEOUT: Duration = Duration::from_secs(30);
/// stop 请求的等待超时（10 秒），超时则放弃并报错。
const STOP_TIMEOUT: Duration = Duration::from_secs(10);
/// 获取子代理输出的默认超时（30 秒）。
const DEFAULT_OUTPUT_TIMEOUT_MS: u64 = 30_000;
/// 获取子代理输出的最大超时上限（10 分钟）。
const MAX_OUTPUT_TIMEOUT_MS: u64 = 600_000;

/// 一次 `TaskExecute` 记下的级联配置。
#[derive(Debug, Clone)]
pub struct CascadeConfig {
    /// 追加到每个 agent prompt 的额外上下文。
    pub additional_context: Option<String>,
    /// agent 的模型覆盖。
    pub model: Option<String>,
    /// 每个 agent 的最大回合数。
    pub max_turns: Option<u64>,
}

/// RPC 与联动运行态。
#[derive(Default)]
pub struct RpcState {
    /// 在途请求：requestId → 回执投递口。
    pending: BTreeMap<String, Sender<Value>>,
    /// agentId → taskId（O(1) 终结查找）。
    pub agent_task_map: BTreeMap<String, String>,
    /// 等待某 agent 终结的等候者。
    agent_waiters: BTreeMap<String, Vec<Sender<()>>>,
    /// subagent 是否可用（协议版本一致）。
    pub available: bool,
    /// 是否已经 ping 过（避免每次 exec ctx 重发）。
    pinged: bool,
    /// 已就版本问题提示过，避免重复打扰。
    warned: bool,
    /// 最近一次 `TaskExecute` 的级联配置。
    pub cascade: Option<CascadeConfig>,
}

/// 扩展事件入口（由门面 `on_extension_event` 调用；**同步**）。
pub fn on_event(name: &str, payload: &Value) {
    // ping 回执：只带版本，不走在途请求表。
    if name.starts_with("subagents:rpc:ping:reply:") {
        on_ping_reply(payload.pointer("/data/version").and_then(Value::as_u64));
        return;
    }

    // 通用回执：`<channel>:reply:<requestId>`
    if let Some(idx) = name.rfind(":reply:") {
        let request_id = &name[idx + ":reply:".len()..];
        let sender = lock_state().rpc.pending.remove(request_id);

        if let Some(tx) = sender {
            _ = tx.send(payload.clone());
        }

        return;
    }

    match name {
        "subagents:ready" => {
            lock_state().rpc.pinged = false; // 对方（重新）可用：清掉「已 ping」标记再探活。
            ping();
        }
        "subagents:completed" => on_settled(payload, true),
        "subagents:failed" => on_settled(payload, false),
        _ => {}
    }
}

/// 探活并校验协议版本。幂等：已 ping 过则跳过（`subagents:ready` 会重置）。
pub fn ping() {
    {
        let mut st = lock_state();
        if st.rpc.pinged {
            return;
        }
        st.rpc.pinged = true;
    }

    bus::emit(EXT, "subagents:rpc:ping", json!({ "requestId": make_id() }));
}

/// 处理 ping 回执：按对方协议版本判定 tasks/subagents 扩展是否可用，
/// 不可用且尚未告警时发一次 Warning 通知（`None` 表示对方不带版本，视为过旧）。
fn on_ping_reply(version: Option<u64>) {
    let warning = match version {
        Some(v) if v == PROTOCOL_VERSION => None,
        Some(v) if v > PROTOCOL_VERSION => Some(format!(
            "tasks extension is outdated (protocol v{PROTOCOL_VERSION}, subagents extension has v{v}) — please update for task execution support."
        )),
        Some(v) => Some(format!(
            "subagents extension is outdated (protocol v{v}, tasks extension has v{PROTOCOL_VERSION}) — please update for task execution support."
        )),
        None => Some(
            "@subagents extension is outdated — please update for task execution support."
                .to_string(),
        ),
    };

    let mut st = lock_state();
    match warning {
        None => st.rpc.available = true,
        Some(msg) => {
            st.rpc.available = false;
            if !st.rpc.warned {
                st.rpc.warned = true;
                drop(st);

                request_ui(ExtensionUiRequest::NotifyRich {
                    spans: vec![RichSpan::plain(msg)],
                    level: UiNotifyLevel::Warning,
                });
            }
        }
    }
}

/// 从磁盘重建 `agent_task_map`：`agent_task_map` 只在进程内，重载后为空，
/// 而正在跑的 agent 还在跑——它们的完成事件会被丢掉、任务永远停在 in_progress。
/// 所需信息都在盘上：`TaskExecute` 把 agent id 记进了 task metadata。
///
/// 只重连 `in_progress` 的任务。回退到 pending 的任务保留着 `metadata.agentId`，
/// 重连它会让迟到的事件复活用户已重置的工作。
pub fn reattach() {
    let mut st = lock_state();
    let pairs: Vec<(String, String)> = st
        .store
        .list(None)
        .into_iter()
        .filter_map(|t| {
            let agent_id = t.metadata.get("agentId").and_then(Value::as_str)?;
            (t.status == TaskStatus::InProgress).then(|| (agent_id.to_string(), t.id.clone()))
        })
        .collect();

    for (agent_id, task_id) in pairs {
        st.rpc.agent_task_map.insert(agent_id, task_id);
    }
}

/// 发一个 RPC 并等待带内回执。`Err` 覆盖超时、回执丢失与错误信封。
async fn rpc_call(channel: &str, mut params: Value, timeout: Duration) -> Result<Value, String> {
    let request_id = format!("{}-{}", make_id(), next_seq());
    let (tx, rx) = oneshot::channel();
    lock_state().rpc.pending.insert(request_id.clone(), tx);

    if let Some(obj) = params.as_object_mut() {
        obj.insert("requestId".to_string(), Value::String(request_id.clone()));
    }
    bus::emit(EXT, channel, params);

    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(envelope)) => {
            if envelope.get("success").and_then(Value::as_bool) == Some(true) {
                Ok(envelope.get("data").cloned().unwrap_or(Value::Null))
            } else {
                Err(envelope
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("rpc failed")
                    .to_string())
            }
        }
        Ok(Err(_)) => Err(format!("{channel} reply dropped")),
        Err(_) => {
            lock_state().rpc.pending.remove(&request_id);
            Err(format!("{channel} timeout"))
        }
    }
}

/// 取一个进程内单调递增的序号，用于拼出唯一的 RPC requestId。
fn next_seq() -> u64 {
    /// 生成递增请求 id 的原子序列计数器。
    static SEQ: AtomicU64 = AtomicU64::new(0);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

/// 经 RPC 派发一个后台子代理，返回 agent id。
async fn spawn_agent(
    type_name: &str,
    prompt: String,
    description: String,
    max_turns: Option<u64>,
    model: Option<&str>,
) -> Result<String, String> {
    let mut options = json!({ "description": description, "isBackground": true });
    if let Some(mt) = max_turns {
        options["maxTurns"] = json!(mt);
    }

    if let Some(m) = model.filter(|m| !m.is_empty()) {
        options["model"] = json!(m);
    }

    let data = rpc_call(
        "subagents:rpc:spawn",
        json!({ "type": type_name, "prompt": prompt, "options": options }),
        SPAWN_TIMEOUT,
    )
    .await?;

    data.get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "spawn reply missing id".to_string())
}

/// 停止一个运行中的子代理（错误忽略——停一个已终结的 agent 不是错误）。
pub(super) async fn stop_agent(agent_id: &str) {
    _ = rpc_call(
        "subagents:rpc:stop",
        json!({ "agentId": agent_id }),
        STOP_TIMEOUT,
    )
    .await;
}

/// 硬中止（Esc/Ctrl+C/`AbortRun`/`RestartRun`）收尾：本轮派发出去的后台任务不能悬在
/// `in_progress`——否则用户看到「Operation aborted」，dock 里的任务行却还在计时、转圈。
///
/// 退回 `Pending`（对齐失败回退）：这一轮被取消了，任务并没做完，可以重新 `TaskExecute`。
/// 同时摘掉 `agent_task_map`：迟到的 `subagents:completed`/`failed` 不该再改这个已判定的任务、
/// 也不该触发级联——这一轮被取消了，被它挡住的下游任务不该自动开跑。
///
/// 返回需一并停掉的子代理 id（真正停止靠 RPC；这里只保证界面立即收敛）。
pub(super) fn abort_running() -> Vec<String> {
    let mut st = lock_state();
    let running: Vec<(String, Option<String>)> = st
        .store
        .list(None)
        .into_iter()
        .filter(|t| t.status == TaskStatus::InProgress)
        .map(|t| {
            let agent_id = t
                .metadata
                .get("agentId")
                .and_then(Value::as_str)
                .map(str::to_string);
            (t.id, agent_id)
        })
        .collect();

    let mut agents = Vec::new();

    for (task_id, agent_id) in running {
        // 清掉上一次运行的旧结果，并记下中止原因（与失败回退同一口径）。
        let mut metadata = st
            .store
            .get(&task_id)
            .map(|t| t.metadata.clone())
            .unwrap_or_default();
        metadata.insert("result".to_string(), Value::Null);
        metadata.insert(
            "lastError".to_string(),
            Value::String("aborted".to_string()),
        );

        _ = st.store.update(
            &task_id,
            TaskUpdateFields {
                status: Some(TaskStatusOrDeleted::Status(TaskStatus::Pending)),
                metadata: Some(metadata),
                ..Default::default()
            },
        );
        st.widget.set_active(&task_id, false);

        if let Some(agent_id) = agent_id {
            st.rpc.agent_task_map.remove(&agent_id);
            notify_waiters(&mut st, &agent_id);
            agents.push(agent_id);
        }
    }

    // 任务转回未完成：整批倒计时不再成立。
    st.auto_clear.reset_batch_countdown();
    agents
}

/// 告诉 subagent 其结果已被交给模型，抑制它为此 agent 保留的完成通知。
///
/// fire-and-forget，且刻意不在版本握手内：没有该处理器的 subagents 会继续通知，而不是让读取失败。
pub fn consume_result(agent_id: &str) {
    bus::emit(
        EXT,
        "subagents:rpc:consume",
        json!({ "requestId": make_id(), "agentId": agent_id }),
    );
}

/// 等待一个 agent 终结，带超时。返回是否已终结（超时返回 false）。
async fn wait_agent(agent_id: &str, timeout: Duration) -> bool {
    let (tx, rx) = oneshot::channel();

    {
        // 已从映射里消失 = 已终结，不必等。
        let mut st = lock_state();
        if !st.rpc.agent_task_map.contains_key(agent_id) {
            return true;
        }

        st.rpc
            .agent_waiters
            .entry(agent_id.to_string())
            .or_default()
            .push(tx);
    }

    matches!(tokio::time::timeout(timeout, rx).await, Ok(Ok(())))
}

/// 唤醒所有等待该 agent 终结的 `wait_agent` 任务（移出等待表并逐个 send）。
fn notify_waiters(st: &mut State, agent_id: &str) {
    if let Some(waiters) = st.rpc.agent_waiters.remove(agent_id) {
        for tx in waiters {
            _ = tx.send(());
        }
    }
}

/// 处理 `subagents:completed` / `subagents:failed`。
///
/// `failed` 的 `status` 取值：`error` / `stopped` / `aborted`。**有意停止集合 = stopped + aborted**
fn on_settled(payload: &Value, completed: bool) {
    let Some(agent_id) = payload.get("id").and_then(Value::as_str) else {
        return;
    };

    let agent_id = agent_id.to_string();
    let result = payload
        .get("result")
        .and_then(Value::as_str)
        .map(str::to_string);
    let status = payload
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or(if completed { "completed" } else { "error" })
        .to_string();

    let task_id = {
        let mut st = lock_state();
        notify_waiters(&mut st, &agent_id);

        let Some(task_id) = st.rpc.agent_task_map.remove(&agent_id) else {
            return;
        };

        let Some(task) = st.store.get(&task_id) else {
            return;
        };

        let intentional_stop = completed || status == "stopped" || status == "aborted";

        if intentional_stop {
            let mut metadata = task.metadata.clone();
            if let Some(r) = &result {
                metadata.insert("result".to_string(), Value::String(r.clone()));
            }

            _ = st.store.update(
                &task_id,
                TaskUpdateFields {
                    status: Some(TaskStatusOrDeleted::Status(TaskStatus::Completed)),
                    metadata: Some(metadata),
                    ..Default::default()
                },
            );

            st.widget.set_active(&task_id, false);

            let turn = st.cadence.current_turn;
            let mode = st.config.auto_clear_completed();
            let State {
                store, auto_clear, ..
            } = &mut *st;
            auto_clear.track_completion(store, &task_id, turn, mode);
        } else {
            // 回退 pending。`result: null` 会删键：回到 pending 的任务没有当前结果，
            // 上一次运行的旧结果不该在它被读取的每处压过这次错误。
            let mut metadata = task.metadata.clone();
            metadata.insert("result".to_string(), Value::Null);
            metadata.insert(
                "lastError".to_string(),
                Value::String(
                    payload
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or(&status)
                        .to_string(),
                ),
            );

            _ = st.store.update(
                &task_id,
                TaskUpdateFields {
                    status: Some(TaskStatusOrDeleted::Status(TaskStatus::Pending)),
                    metadata: Some(metadata),
                    ..Default::default()
                },
            );
            st.widget.set_active(&task_id, false);
            st.auto_clear.reset_batch_countdown();
        }
        task_id
    };

    extensions::request_show_dock();

    // 级联：终结任务解锁的后续任务自动开跑（异步，不阻塞总线分发）。
    // 事件可能从无 runtime 的线程投递（会话切换），故先探测 runtime。
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(cascade_from(task_id));
    }
}

/// `TaskExecute`
pub async fn execute(args: &Value) -> Result<ToolResult, ToolError> {
    if !lock_state().rpc.available {
        return Ok(ToolResult::text(
            "Subagent execution is currently unavailable (@tintinweb/pi-subagents not loaded \
             or version mismatch). You can run these as plain Agent-tool spawns, but pi-tasks \
             won't track them — status stays pending, cascade won't fire, TaskOutput stays empty.",
        ));
    }

    let task_ids: Vec<String> = args
        .get("task_ids")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let additional = args
        .get("additional_context")
        .and_then(Value::as_str)
        .map(str::to_string);
    let model = args
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_string);
    let max_turns = args.get("max_turns").and_then(Value::as_u64);

    let mut results: Vec<String> = Vec::new();
    let mut launched: Vec<String> = Vec::new();

    for task_id in task_ids {
        // 校验（短锁）
        let (task, agent_type, blocked) = {
            let mut st = lock_state();
            let Some(task) = st.store.get(&task_id) else {
                results.push(format!("#{task_id}: not found"));
                continue;
            };

            if task.status != TaskStatus::Pending {
                results.push(format!(
                    "#{task_id}: not pending (status: {})",
                    task.status.as_str()
                ));
                continue;
            }

            let Some(agent_type) = task
                .metadata
                .get("agentType")
                .and_then(Value::as_str)
                .map(str::to_string)
            else {
                results.push(format!(
                    "#{task_id}: no agentType set — create with agentType parameter or update metadata"
                ));
                continue;
            };

            let open: Vec<String> = task
                .blocked_by
                .iter()
                .filter(|bid| {
                    st.store
                        .get(bid)
                        .map(|b| b.status != TaskStatus::Completed)
                        .unwrap_or(true)
                })
                .map(|id| format!("#{id}"))
                .collect();

            (task, agent_type, open)
        };

        if !blocked.is_empty() {
            results.push(format!("#{task_id}: blocked by {}", blocked.join(", ")));
            continue;
        }

        // 标记 in_progress + 构造 prompt
        let prompt = {
            let mut st = lock_state();
            _ = st.store.update(
                &task_id,
                TaskUpdateFields {
                    status: Some(TaskStatusOrDeleted::Status(TaskStatus::InProgress)),
                    ..Default::default()
                },
            );

            build_task_prompt(&task, additional.as_deref(), &st.store)
        };

        match spawn_agent(
            &agent_type,
            prompt,
            task.subject.clone(),
            max_turns,
            model.as_deref(),
        )
        .await
        {
            Ok(agent_id) => {
                let mut st = lock_state();
                st.rpc
                    .agent_task_map
                    .insert(agent_id.clone(), task_id.clone());
                let mut metadata = task.metadata.clone();
                metadata.insert("agentId".to_string(), Value::String(agent_id.clone()));

                _ = st.store.update(
                    &task_id,
                    TaskUpdateFields {
                        owner: Some(agent_id.clone()),
                        metadata: Some(metadata),
                        ..Default::default()
                    },
                );
                st.widget.set_active(&task_id, true);
                launched.push(format!("#{task_id} → agent {agent_id}"));
            }
            Err(e) => {
                _ = lock_state().store.update(
                    &task_id,
                    TaskUpdateFields {
                        status: Some(TaskStatusOrDeleted::Status(TaskStatus::Pending)),
                        ..Default::default()
                    },
                );
                results.push(format!("#{task_id}: spawn failed — {e}"));
            }
        }
    }

    // 记下本次的级联配置：完成监听器要用。
    lock_state().rpc.cascade = Some(CascadeConfig {
        additional_context: additional,
        model,
        max_turns,
    });
    extensions::request_show_dock();

    let mut lines: Vec<String> = Vec::new();
    if !launched.is_empty() {
        lines.push(format!(
            "Launched {} agent(s):\n{}\nUse TaskOutput to check progress. Do not spawn additional agents for these tasks.",
            launched.len(),
            launched.join("\n")
        ));
    }

    if !results.is_empty() {
        lines.push(format!("Skipped:\n{}", results.join("\n")));
    }

    if lines.is_empty() {
        lines.push("No tasks to execute.".to_string());
    }

    Ok(ToolResult::text(lines.join("\n\n")))
}

/// `TaskOutput`
pub async fn output(args: &Value) -> Result<ToolResult, ToolError> {
    let raw = args
        .get("task_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ToolError("task_id is required".to_string()))?;
    let block = args.get("block").and_then(Value::as_bool).unwrap_or(true);
    let timeout_ms = args
        .get("timeout")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_OUTPUT_TIMEOUT_MS)
        .min(MAX_OUTPUT_TIMEOUT_MS);

    let resolved = resolve_id(raw);
    let task = lock_state().store.get(&resolved);
    let Some(task) = task else {
        return Err(ToolError(format!("No task found with ID {raw}")));
    };

    let Some(agent_id) = task
        .metadata
        .get("agentId")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return Err(ToolError(format!("No background process for task {raw}")));
    };

    if block && task.status == TaskStatus::InProgress {
        _ = wait_agent(&agent_id, Duration::from_millis(timeout_ms)).await;
    }

    let updated = lock_state().store.get(&resolved).unwrap_or(task);

    // 只消费真正交出去的东西：agent 已回报（离开映射）且任务带上其结局。
    // 两者缺一——仍在跑，或更新没落地——模型拿到的只是状态，
    // 而 subagent 持有的通知是唯一会宣告结果的途径。
    if !lock_state().rpc.agent_task_map.contains_key(&agent_id)
        && updated.status != TaskStatus::InProgress
    {
        consume_result(&agent_id);
    }

    let output = updated
        .metadata
        .get("result")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            updated
                .metadata
                .get("lastError")
                .and_then(Value::as_str)
                .map(|e| format!("Error: {e}"))
        });

    Ok(ToolResult::text(format!(
        "Task #{resolved} [{}] — subagent {agent_id}{}",
        updated.status.as_str(),
        output.map(|o| format!("\n\n{o}")).unwrap_or_default()
    )))
}

/// `TaskStop`
pub async fn stop(args: &Value) -> Result<ToolResult, ToolError> {
    let raw = args
        .get("task_id")
        .or_else(|| args.get("shell_id"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ToolError("task_id is required".to_string()))?;

    let resolved = resolve_id(raw);
    let task = lock_state().store.get(&resolved);
    let agent_id = task.as_ref().and_then(|t| {
        t.metadata
            .get("agentId")
            .and_then(Value::as_str)
            .map(str::to_string)
    });
    let running = task
        .as_ref()
        .map(|t| t.status == TaskStatus::InProgress)
        .unwrap_or(false);

    let Some(agent_id) = agent_id.filter(|_| running) else {
        return Err(ToolError(format!(
            "No running background process for task {raw}"
        )));
    };

    {
        let mut st = lock_state();
        _ = st.store.update(
            &resolved,
            TaskUpdateFields {
                status: Some(TaskStatusOrDeleted::Status(TaskStatus::Completed)),
                ..Default::default()
            },
        );
        st.widget.set_active(&resolved, false);

        let turn = st.cadence.current_turn;
        let mode = st.config.auto_clear_completed();
        let State {
            store, auto_clear, ..
        } = &mut *st;
        auto_clear.track_completion(store, &resolved, turn, mode);
    }

    stop_agent(&agent_id).await;
    extensions::request_show_dock();

    Ok(ToolResult::text(format!(
        "Task #{resolved} stopped successfully"
    )))
}

/// 把 agent id（含部分前缀）解析为 task id；已是 task id 则原样返回。
fn resolve_id(raw: &str) -> String {
    let st = lock_state();
    if st.store.cached_get(raw).is_some() {
        return raw.to_string();
    }

    for (agent_id, task_id) in &st.rpc.agent_task_map {
        if agent_id == raw || agent_id.starts_with(raw) {
            return task_id.clone();
        }
    }
    raw.to_string()
}

/// 一个任务完成后，若开启了 auto-cascade，则自动开跑其解锁的、带 agentType 的后续任务。
async fn cascade_from(completed_task_id: String) {
    let (enabled, cascade) = {
        let st = lock_state();
        (st.config.auto_cascade(), st.rpc.cascade.clone())
    };

    if !enabled {
        return;
    }

    let Some(cascade) = cascade else {
        return;
    };

    // 找出：pending、有 agentType、blocked_by 含刚完成的、且所有 blockedBy 都 completed
    let dependents: Vec<(String, String, String, Task)> = {
        let mut st = lock_state();
        let all = st.store.list(None);
        all.into_iter()
            .filter(|t| t.status == TaskStatus::Pending)
            .filter(|t| t.blocked_by.contains(&completed_task_id))
            .filter(|t| {
                t.blocked_by.iter().all(|d| {
                    st.store
                        .cached_get(d)
                        .map(|x| x.status == TaskStatus::Completed)
                        .unwrap_or(false)
                })
            })
            .filter_map(|t| {
                let agent_type = t.metadata.get("agentType").and_then(Value::as_str)?;
                Some((t.id.clone(), agent_type.to_string(), t.subject.clone(), t))
            })
            .collect()
    };

    for (task_id, agent_type, subject, task) in dependents {
        let prompt = {
            let mut st = lock_state();
            _ = st.store.update(
                &task_id,
                TaskUpdateFields {
                    status: Some(TaskStatusOrDeleted::Status(TaskStatus::InProgress)),
                    ..Default::default()
                },
            );
            build_task_prompt(&task, cascade.additional_context.as_deref(), &st.store)
        };

        match spawn_agent(
            &agent_type,
            prompt,
            subject,
            cascade.max_turns,
            cascade.model.as_deref(),
        )
        .await
        {
            Ok(agent_id) => {
                let mut st = lock_state();
                st.rpc
                    .agent_task_map
                    .insert(agent_id.clone(), task_id.clone());

                let mut metadata = task.metadata.clone();
                metadata.insert("agentId".to_string(), Value::String(agent_id.clone()));

                _ = st.store.update(
                    &task_id,
                    TaskUpdateFields {
                        owner: Some(agent_id),
                        metadata: Some(metadata),
                        ..Default::default()
                    },
                );
                st.widget.set_active(&task_id, true);
            }
            Err(e) => {
                let mut st = lock_state();
                let mut metadata = task.metadata.clone();
                metadata.insert("lastError".to_string(), Value::String(e));

                _ = st.store.update(
                    &task_id,
                    TaskUpdateFields {
                        status: Some(TaskStatusOrDeleted::Status(TaskStatus::Pending)),
                        metadata: Some(metadata),
                        ..Default::default()
                    },
                );
            }
        }
    }
    extensions::request_show_dock();
}

/// 构造交给子代理的 prompt。注入已完成的依赖结果，使级联的 agent 直接建立在前置成果之上、无需重新获取。
pub fn build_task_prompt(
    task: &Task,
    additional_context: Option<&str>,
    store: &TaskStore,
) -> String {
    let mut prompt = format!(
        "You are executing task #{}: \"{}\"\n\n{}",
        task.id, task.subject, task.description
    );

    if !task.blocked_by.is_empty() {
        let mut deps: Vec<String> = Vec::new();
        for dep_id in &task.blocked_by {
            let Some(dep) = store.cached_get(dep_id) else {
                continue;
            };

            let Some(result) = dep.metadata.get("result").and_then(Value::as_str) else {
                continue;
            };

            let result = if result.chars().count() > 4000 {
                format!(
                    "{}\n\n[... truncated — use TaskGet for full output]",
                    result.chars().take(4000).collect::<String>()
                )
            } else {
                result.to_string()
            };
            deps.push(format!("### Task #{dep_id}: {}\n{result}", dep.subject));
        }

        if !deps.is_empty() {
            prompt.push_str(&format!(
                "\n\n## Prerequisite task results\n\n{}",
                deps.join("\n\n")
            ));
        }
    }

    if let Some(ctx) = additional_context.filter(|c| !c.is_empty()) {
        prompt.push_str(&format!("\n\n{ctx}"));
    }

    prompt.push_str("\n\nComplete this task fully. Do not attempt to manage tasks yourself.");
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_includes_dependency_results() {
        let mut store = TaskStore::new(None);
        store.create("Dep".into(), "d".into(), None, None).unwrap();
        store
            .create("Child".into(), "d".into(), None, None)
            .unwrap();
        let mut meta = serde_json::Map::new();
        meta.insert("result".to_string(), Value::String("dep output".into()));
        store
            .update(
                "1",
                TaskUpdateFields {
                    status: Some(TaskStatusOrDeleted::Status(TaskStatus::Completed)),
                    metadata: Some(meta),
                    ..Default::default()
                },
            )
            .unwrap();
        store
            .update(
                "2",
                TaskUpdateFields {
                    add_blocked_by: Some(vec!["1".into()]),
                    ..Default::default()
                },
            )
            .unwrap();
        let child = store.get("2").unwrap();
        let prompt = build_task_prompt(&child, Some("extra ctx"), &store);
        assert!(prompt.contains("You are executing task #2"));
        assert!(prompt.contains("## Prerequisite task results"));
        assert!(prompt.contains("dep output"));
        assert!(prompt.contains("extra ctx"));
    }

    fn test_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn resolve_id_matches_agent_prefix() {
        let _g = test_lock();
        let mut st = lock_state();
        st.store = TaskStore::new(None);
        st.rpc.agent_task_map.clear();
        st.rpc
            .agent_task_map
            .insert("abcdef123".to_string(), "7".to_string());
        drop(st);
        assert_eq!(resolve_id("abc"), "7");
        assert_eq!(resolve_id("zzz"), "zzz");
    }

    /// 把全局 State 重置为内存 store 并登记一个带 agentId 的任务（调用方持 [`test_lock`]）。
    fn seed_running_task() -> String {
        let mut st = lock_state();
        st.store = TaskStore::new(None);
        st.rpc.agent_task_map.clear();
        st.widget.reset();
        st.auto_clear.reset();
        let task = st.store.create("T".into(), "d".into(), None, None).unwrap();
        let mut meta = serde_json::Map::new();
        meta.insert("agentId".to_string(), Value::String("agent-1".into()));
        st.store
            .update(
                &task.id,
                TaskUpdateFields {
                    status: Some(TaskStatusOrDeleted::Status(TaskStatus::InProgress)),
                    metadata: Some(meta),
                    ..Default::default()
                },
            )
            .unwrap();
        st.rpc
            .agent_task_map
            .insert("agent-1".to_string(), task.id.clone());
        task.id
    }

    fn task_status(id: &str) -> (TaskStatus, Option<String>) {
        let mut st = lock_state();
        let t = st.store.get(id).unwrap();
        (
            t.status,
            t.metadata
                .get("result")
                .and_then(Value::as_str)
                .map(str::to_string),
        )
    }

    #[test]
    fn completed_event_marks_done_and_stores_result() {
        let _g = test_lock();
        let id = seed_running_task();
        on_event(
            "subagents:completed",
            &json!({ "id": "agent-1", "result": "all done", "status": "completed" }),
        );
        let (status, result) = task_status(&id);
        assert_eq!(status, TaskStatus::Completed);
        assert_eq!(result.as_deref(), Some("all done"));
    }

    #[test]
    fn failed_error_reverts_to_pending() {
        let _g = test_lock();
        let id = seed_running_task();
        on_event(
            "subagents:failed",
            &json!({ "id": "agent-1", "error": "boom", "status": "error" }),
        );
        let (status, _) = task_status(&id);
        assert_eq!(status, TaskStatus::Pending);
        let mut st = lock_state();
        let t = st.store.get(&id).unwrap();
        assert_eq!(
            t.metadata.get("lastError").and_then(Value::as_str),
            Some("boom")
        );
        assert!(t.metadata.get("result").is_none(), "回退时清掉旧结果");
    }

    #[test]
    fn aborted_is_treated_as_intentional_stop() {
        // prux 的 subagent 把 max_turns 宽限耗尽的 aborted 也发在 failed 上；
        // 上游只认 stopped，照搬会把「预算耗尽」误判为错误。
        let _g = test_lock();
        let id = seed_running_task();
        on_event(
            "subagents:failed",
            &json!({ "id": "agent-1", "result": "partial", "status": "aborted" }),
        );
        let (status, result) = task_status(&id);
        assert_eq!(status, TaskStatus::Completed);
        assert_eq!(result.as_deref(), Some("partial"));
    }

    #[test]
    fn abort_running_reverts_to_pending_and_detaches_agents() {
        // 硬中止（Esc）：任务不能悬在 in_progress（dock 会一直计时/转圈），应退回 Pending
        // 可重新 TaskExecute；且要摘掉映射——迟到的终结事件不该再动这个已判定的任务。
        let _g = test_lock();
        let id = seed_running_task();

        // 上一次运行遗留的 result 必须被清掉（story 会压过这次的中止）
        {
            let mut st = lock_state();
            let mut meta = serde_json::Map::new();
            meta.insert("result".to_string(), Value::String("stale".into()));
            st.store
                .update(
                    &id,
                    TaskUpdateFields {
                        metadata: Some(meta),
                        ..Default::default()
                    },
                )
                .unwrap();
        }

        let agents = abort_running();
        assert_eq!(agents, vec!["agent-1".to_string()]);

        let (status, result) = task_status(&id);
        assert_eq!(status, TaskStatus::Pending, "中止后应退回待办");
        assert!(result.is_none(), "中止不是完成，不该保留 result");

        let mut st = lock_state();
        assert_eq!(
            st.store
                .get(&id)
                .unwrap()
                .metadata
                .get("lastError")
                .and_then(Value::as_str),
            Some("aborted")
        );
        assert!(
            !st.rpc.agent_task_map.contains_key("agent-1"),
            "已判定的任务不应再接收迟到事件"
        );

        // 迟到的终态事件于是成为 no-op（任务状态不被改写）
        drop(st);
        on_event(
            "subagents:failed",
            &json!({ "id": "agent-1", "error": "late", "status": "error" }),
        );
        assert_eq!(task_status(&id).0, TaskStatus::Pending);
    }

    #[test]
    fn abort_running_is_noop_without_in_progress_tasks() {
        let _g = test_lock();
        {
            let mut st = lock_state();
            st.store = TaskStore::new(None);
            st.rpc.agent_task_map.clear();
            st.store.create("P".into(), "d".into(), None, None).unwrap();
        }
        assert!(abort_running().is_empty());
    }

    #[test]
    fn reattach_links_in_progress_tasks_only() {
        let _g = test_lock();
        {
            let mut st = lock_state();
            st.store = TaskStore::new(None);
            st.rpc.agent_task_map.clear();
            let running = st.store.create("R".into(), "d".into(), None, None).unwrap();
            let pending = st.store.create("P".into(), "d".into(), None, None).unwrap();
            for (id, agent) in [(&running.id, "a-run"), (&pending.id, "a-pend")] {
                let mut meta = serde_json::Map::new();
                meta.insert("agentId".to_string(), Value::String(agent.into()));
                let status = if *id == running.id {
                    TaskStatus::InProgress
                } else {
                    TaskStatus::Pending
                };
                st.store
                    .update(
                        id,
                        TaskUpdateFields {
                            status: Some(TaskStatusOrDeleted::Status(status)),
                            metadata: Some(meta),
                            ..Default::default()
                        },
                    )
                    .unwrap();
            }
        }
        reattach();
        let st = lock_state();
        assert!(st.rpc.agent_task_map.contains_key("a-run"));
        assert!(
            !st.rpc.agent_task_map.contains_key("a-pend"),
            "pending 任务不重连（防迟到事件复活已重置的工作）"
        );
    }
}
