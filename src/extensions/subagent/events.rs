//! 跨扩展事件与 RPC
//!
//! 出站：把 `subagents:*` 生命周期事件发到核心事件总线（[`crate::core::extensions::events::emit`]）。
//! 入站：核心把总线事件交回 [`on_event`]，这里处理 `subagents:rpc:*` 四个方法，
//! 回执发到 `"<channel>:reply:<requestId>"`（信封 `{success,data?}` / `{success,error}`）。
//!
//! 只有**顶层**记录（非嵌套、非工作流派发）发生命周期事件、才允许被 RPC 停

use super::{
    EXT, agent_types, discovery_opts, fleet, manager,
    model::{self, ModelSource, ScopeVerdict},
    session,
    types::AgentRecord,
    types::AgentStatus,
};
use crate::core::extensions::{ToolExecCtx, events as bus};
use serde_json::{Value, json};

/// RPC 协议版本（信封或方法契约变更时递增）。
pub const PROTOCOL_VERSION: u32 = 2;

/// 顶层记录：主会话直接派的（非嵌套、非工作流派发）。
pub(super) fn is_top_level(rec: &AgentRecord) -> bool {
    rec.parent_agent_id.is_none() && !rec.workflow_owned
}

/// 发一个出站事件（发送方固定为本扩展）。
fn emit(name: &str, payload: Value) {
    bus::emit(EXT, name, payload);
}

/// 宣告子代理扩展就绪（空载荷，供核心发现 PROTOCOL_VERSION）。
pub(crate) fn ready() {
    emit("subagents:ready", json!({}));
}

/// 新建顶层记录时广播（嵌套 / 工作流派发的记录不发）。
pub(crate) fn created(rec: &AgentRecord) {
    if !is_top_level(rec) {
        return;
    }

    emit(
        "subagents:created",
        json!({
            "id": rec.id,
            "type": rec.agent_type,
            "description": rec.description,
            "isBackground": rec.background,
        }),
    );
}

/// agent 开始执行时广播（仅顶层记录）。
pub(crate) fn started(rec: &AgentRecord) {
    if !is_top_level(rec) {
        return;
    }

    emit(
        "subagents:started",
        json!({
            "id": rec.id,
            "type": rec.agent_type,
            "description": rec.description,
        }),
    );
}

/// 用户对运行中的 agent 发送 steer 消息时广播。
pub(crate) fn steered(id: &str, message: &str) {
    emit("subagents:steered", json!({ "id": id, "message": message }));
}

/// agent 上下文被压缩后广播（仅顶层记录；带上压缩原因、压缩前 token 数与累计压缩次数）。
pub(crate) fn compacted(rec: &AgentRecord, reason: &str, tokens_before: u64) {
    if !is_top_level(rec) {
        return;
    }

    emit(
        "subagents:compacted",
        json!({
            "id": rec.id,
            "type": rec.agent_type,
            "description": rec.description,
            "reason": reason,
            "tokensBefore": tokens_before,
            "compactionCount": rec.compaction_count,
        }),
    );
}

/// 终态：按状态发 `subagents:completed` 或 `subagents:failed`（顶层才发）。
pub(crate) fn settled(rec: &AgentRecord) {
    if !is_top_level(rec) {
        return;
    }

    let failed = matches!(
        rec.status,
        AgentStatus::Error | AgentStatus::Stopped | AgentStatus::Aborted
    );

    let name = if failed {
        "subagents:failed"
    } else {
        "subagents:completed"
    };

    emit(name, lifecycle_payload(rec));
}

/// 状态 + 结果 + 用量（tokens / usage）。
fn lifecycle_payload(rec: &AgentRecord) -> Value {
    let u = rec.usage;
    let total = u.display_total();

    let tokens =
        (total > 0).then(|| json!({ "input": u.input, "output": u.output, "total": total }));

    let usage =
        (u.input + u.output + u.cache_read + u.cache_write > 0 || u.cost > 0.0).then(|| {
            json!({
                "input": u.input,
                "output": u.output,
                "cacheRead": u.cache_read,
                "cacheWrite": u.cache_write,
                "totalTokens": u.input + u.output + u.cache_read + u.cache_write,
                "cost": { "total": u.cost },
            })
        });

    let mut payload = json!({
        "id": rec.id,
        "type": rec.agent_type,
        "description": rec.description,
        "result": rec.result,
        "status": rec.status.as_str(),
        "toolUses": rec.tool_uses,
        "durationMs": rec.duration_ms(),
    });

    if let Some(t) = tokens {
        payload["tokens"] = t;
    }

    if let Some(us) = usage {
        payload["usage"] = us;
    }
    payload
}

/// 调度变更事件（`ScheduleChangeEvent`）。
pub(crate) fn schedule_changed(event: Value) {
    emit("subagents:scheduled", event);
}

/// 调度器就绪并带上当前待执行任务数（含 sessionId，供前端按会话过滤）。
pub(crate) fn scheduler_ready(job_count: usize) {
    let session_id = session::session_key();

    emit(
        "subagents:scheduler_ready",
        json!({ "sessionId": session_id, "jobCount": job_count }),
    );
}

/// 处理一条总线事件（由 `Extension::on_extension_event` 调用）。
pub(crate) fn on_event(name: &str, payload: &Value) {
    match name {
        "subagents:rpc:ping" => reply(
            name,
            payload,
            Ok(Some(json!({ "version": PROTOCOL_VERSION }))),
        ),
        "subagents:rpc:spawn" => rpc_spawn(name, payload),
        "subagents:rpc:stop" => rpc_stop(name, payload),
        "subagents:rpc:consume" => rpc_consume(name, payload),
        _ => {}
    }
}

/// 发回执：`"<channel>:reply:<requestId>"`，信封 `{success,data?}` / `{success,error}`。
fn reply(channel: &str, req: &Value, result: Result<Option<Value>, String>) {
    let Some(request_id) = req.get("requestId").and_then(|v| v.as_str()) else {
        return;
    };
    let envelope = match result {
        Ok(Some(data)) => json!({ "success": true, "data": data }),
        Ok(None) => json!({ "success": true }),
        Err(e) => json!({ "success": false, "error": e }),
    };
    emit(&format!("{channel}:reply:{request_id}"), envelope);
}

/// `spawn`：顶层后台派发。需要活跃会话（用扩展缓存的执行上下文）。
///
/// 异步完成（物化/等待启动），所以把整个流程丢进运行时任务，完成后 emit 回执。
fn rpc_spawn(channel: &str, req: &Value) {
    let request_id = req
        .get("requestId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let reply_channel = format!("{channel}:reply:{request_id}");

    let Some((ctx, rt)) = fleet::cached_ctx() else {
        emit(
            &reply_channel,
            json!({ "success": false, "error": "No active session" }),
        );
        return;
    };

    let type_name = req
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let prompt = req
        .get("prompt")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let options = req.get("options").cloned().unwrap_or(Value::Null);

    rt.spawn(async move {
        let envelope = match spawn_inner(&ctx, &type_name, &prompt, &options).await {
            Ok(id) => json!({ "success": true, "data": { "id": id } }),
            Err(e) => json!({ "success": false, "error": e }),
        };
        emit(&reply_channel, envelope);
    });
}

/// RPC spawn 的主体：解析类型 → 校验 options.model → 顶层后台 dispatch。
async fn spawn_inner(
    ctx: &ToolExecCtx,
    type_name: &str,
    prompt: &str,
    options: &Value,
) -> Result<String, String> {
    let cfg = manager::config();
    let roster = agent_types::discover_with(&ctx.cwd, discovery_opts(&cfg));
    let ty = agent_types::resolve(&roster.types, type_name)
        .map_err(|e| format!("agent type {type_name:?}: {e:?}"))?
        .clone();

    let mut req = manager::request_for_type(
        &ty,
        prompt.to_string(),
        options
            .get("description")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| super::mention::describe_mention(prompt)),
        None,
    );

    // RPC 总是顶层、后台
    req.run_in_background = true;
    req.parent_agent_id = None;
    req.depth = 0;
    req.workflow_owned = false;

    if let Some(mt) = options.get("maxTurns").and_then(|v| v.as_u64()) {
        req.max_turns = Some(mt as u32);
    }

    // `options.model`：JSON 转发方会把未设字段序列化成 null，null = 继承（不是覆盖）。
    if let Some(raw) = options.get("model").filter(|v| !v.is_null()) {
        let label = raw
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| raw.to_string());
        let canonical = model::resolve(&label)?;

        match model::check_scope(
            cfg.scope_models,
            Some(&canonical),
            ModelSource::Caller,
            ty.label(),
            Some(&label),
        ) {
            ScopeVerdict::Ok => {}
            ScopeVerdict::Refuse(message) => return Err(message),
            ScopeVerdict::Warn(_) => {}
        }

        req.model = Some(canonical);
        req.model_source = ModelSource::Caller;
    }

    let dispatched = manager::dispatch(ctx, &ty, req).await?;
    Ok(dispatched.id)
}

/// `stop`：只允许停顶层记录。
fn rpc_stop(channel: &str, req: &Value) {
    let result = (|| -> Result<Option<Value>, String> {
        let id = req.get("agentId").and_then(|v| v.as_str()).unwrap_or("");
        let rec = manager::record(id).ok_or_else(|| "Agent not found".to_string())?;

        if !is_top_level(&rec) {
            return Err("Agent is owned by another agent or workflow".to_string());
        }

        // 查找已证明存在；`stop` 失败只可能是记录已终结。
        manager::stop(id).map_err(|_| "Agent is not running".to_string())?;
        Ok(None)
    })();

    reply(channel, req, result);
}

/// `consume`：把已终结结果标为已读，抑制完成通知。
fn rpc_consume(channel: &str, req: &Value) {
    let result = (|| -> Result<Option<Value>, String> {
        let id = req.get("agentId").and_then(|v| v.as_str()).unwrap_or("");
        if !manager::consume_result(id) {
            return Err("Agent not found or still running".to_string());
        }
        Ok(None)
    })();

    reply(channel, req, result);
}
