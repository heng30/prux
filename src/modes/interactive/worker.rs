//! agent worker：独占持有 `Agent` 的 tokio 任务
//!
//! 架构：UI 线程零直接访问 agent；所有操作经 `AgentCommand` 下发，回执/状态更新
//! 经 `run_in_event_loop` 投闭包直达 `&mut App`（`apply_sink_event`）。
//! - agent 以 `Arc<Mutex<Agent>>` 由 **worker 单线程**持有：std Mutex 仅用于
//!   future 与 worker 轮询分支互斥（同一任务内先取后放，无跨线程竞争）。UI 侧完全看不到这把锁
//! - 回合类命令（Prompt/PromptMessage/PromptBatch/Compact/Navigate）经
//!   `start_round` 借 agent 执行；future 内部 `select!` 监听 abort oneshot，
//!   abort 到达即 drop 底层的 prompt future，agent 完整归还。
//! - busy 期间到达的命令进入排队队列，回合结束后依次执行。
//! - 流式事件经 agent 原有 emit 机制 → JSON sink 只做 `run_in_event_loop` 投闭包（`apply_sink_event`，无锁）。
//! - 命令回执类型化：dispatch 组装类型化载荷后直接 `run_in_event_loop` 投闭包调 `app::on_*`（不再构造/解析 JSON）。

use super::{
    agent_actor::{AgentCommand, CommandRx},
    app::{
        AppliedKey, CostBreakdownEntry, ModelChanged, MsgLevel, NavigateDone, RepairAction,
        RewindDone, SessionStats, SessionSwitched, ZombieOperationsRequest,
    },
    run_in_event_loop,
};
use crate::{
    APP_NAME,
    core::{
        self,
        agent_session::Agent,
        bug_report,
        compaction::CompactionReason,
        crash_log, model_resolver,
        provider::{AgentMessage, ContentBlock, Usage},
        session_manager::{self, OpenSessionResult, Session},
        settings_manager,
        skills::Skill,
    },
    error::{Error, Result},
    modes::interactive::app::SessionRepairRequest,
    utils::time::now_iso,
};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use std::{
    collections::{HashSet, VecDeque},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use tokio::sync::{Mutex, oneshot};

/// 回合 future：借 agent 执行 async 回合，Output 不带 agent（Arc 共享无需归还）
type RoundFuture = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;
/// 回合中断信号的发送端，发出即丢弃底层 prompt future 中止当前回合。
type AbortTx = oneshot::Sender<()>;

/// 无模型归属的用量桶（嵌套工具调用、摘要生成）
const TOOLS_BUCKET: &str = "Tools/summaries";

/// worker 主循环
pub async fn run_worker(agent: Arc<Mutex<Agent>>, mut cmd_rx: CommandRx) {
    // 事件 sink → 闭包队列（无锁）。agent 在 worker 内独占，UI 不再 attach sink。
    agent
        .lock()
        .await
        .attach_json_sink(Box::new(move |ev: Value| {
            _ = run_in_event_loop(move |ui| ui.apply_sink_event(&ev));
        }));

    let mut on_round: Option<RoundFuture> = None;
    let mut pending_abort: Option<AbortTx> = None;
    let mut queue: VecDeque<AgentCommand> = VecDeque::new(); // busy 期间排队的命令（回合结束后处理）

    // 退出收尾：丢弃进行中的回合（其 run_loop 不会再写 finish），
    // 为所有未闭合 operation 补写 interrupted，避免重开会话出现僵尸条目。
    // 返回后 cmd_rx 仍被持有以保持通道，调用方须在 worker 结束后释放。
    let mut shutting_down = false;

    loop {
        if let Some(fut) = on_round.as_mut() {
            tokio::select! {
                cmd = cmd_rx.recv() => {
                    match cmd {
                        None => shutting_down = true,
                        Some(AgentCommand::Abort) => {
                            if let Some(tx) = pending_abort.take() {
                                 _ = tx.send(());
                            }
                        }
                        Some(AgentCommand::Shutdown) => shutting_down = true,
                        Some(c) => queue.push_back(c),
                    }
                }
                _ = fut.as_mut() => {
                     _ = on_round.take();
                     _ = pending_abort.take();

                     // 回合结束：应用保温回执（保温任务在回合期间并行跑）
                     drain_cache_warm(&agent).await;
                }
            }

            if shutting_down {
                _ = on_round.take();
                _ = pending_abort.take();
                finish_all_open_operations(&agent).await;
                break;
            }
        } else {
            // idle：优先消费排队命令，再收新命令
            if let Some(cmd) = queue.pop_front() {
                if matches!(cmd, AgentCommand::Shutdown) {
                    finish_all_open_operations(&agent).await;
                    break;
                }

                if let Some(round) = dispatch(&agent, cmd, &mut pending_abort).await {
                    on_round = Some(round);
                }
                continue;
            }

            let cmd = tokio::select! {
                cmd = cmd_rx.recv() => cmd,
                // 空闲时周期唤醒：应用提示缓存保温回执（idle 模式可能不伴随任何命令）
                _ = tokio::time::sleep(Duration::from_millis(1000)) => {
                    drain_cache_warm(&agent).await;
                    continue;
                }
            };

            let Some(cmd) = cmd else {
                finish_all_open_operations(&agent).await;
                break;
            };

            if matches!(cmd, AgentCommand::Shutdown) {
                finish_all_open_operations(&agent).await;
                break;
            }

            if let Some(round) = dispatch(&agent, cmd, &mut pending_abort).await {
                on_round = Some(round);
            }
        }
    }
}

/// 为会话中所有未闭合 operation 补写 `operation_finished(outcome="interrupted")`。
/// 进程退出前调用：任何进行中/被丢弃的回合都不会再有机会由 run_loop 收尾。
async fn finish_all_open_operations(agent: &Arc<Mutex<Agent>>) {
    let mut g = agent.lock().await;
    if let Some(s) = g.session.as_mut() {
        _ = s.interrupt_all_open_operations();
    }
}

/// 应用提示缓存保温回执：落成 `cache_warm` usage 记录 + 发 `cache_warm` 事件（TUI 通知）。
async fn drain_cache_warm(agent: &Arc<Mutex<Agent>>) {
    let outcomes = agent.lock().await.cache_warmer.drain_outcomes();
    if outcomes.is_empty() {
        return;
    }

    let mut g = agent.lock().await;
    for o in outcomes {
        if let Some(s) = g.session.as_mut() {
            let model = o.response_model.as_deref().unwrap_or(&o.model_id);
            _ = s.append_cache_warm_usage(&o.usage, &o.provider, model);
        }

        let event = json!({
            "type": "cache_warm",
            "cost": o.usage.cost.total,
            "provider": o.provider,
            "modelId": o.model_id,
            "responseModel": o.response_model,
            "extensionOverride": o.extension_override,
            "usage": o.usage,
        });
        g.emit(event);
    }
}

/// 分发单条命令。返回 `Some` 表示进入了回合（挂到 on_round）。
async fn dispatch(
    agent: &Arc<Mutex<Agent>>,
    cmd: AgentCommand,
    pending_abort: &mut Option<AbortTx>,
) -> Option<RoundFuture> {
    match cmd {
        AgentCommand::Prompt(text) => dispatch_prompt(agent, text, pending_abort).await,
        AgentCommand::PromptMessage(msg) => {
            dispatch_prompt_message(agent, msg, pending_abort).await
        }
        AgentCommand::PromptBatch(msgs) => dispatch_prompt_batch(agent, msgs, pending_abort).await,
        AgentCommand::Compact { instructions } => {
            dispatch_compact(agent, instructions, pending_abort).await
        }
        AgentCommand::Navigate {
            target_id,
            summarize,
        } => dispatch_navigate(agent, target_id, summarize, pending_abort).await,
        AgentCommand::Rewind => dispatch_rewind(agent).await,
        AgentCommand::AppendMessage { msg } => dispatch_append_message(agent, msg).await,
        AgentCommand::AppendCustomEntry { custom_type, data } => {
            dispatch_append_custom_entry(agent, custom_type, data).await
        }
        AgentCommand::SetSessionName { name } => dispatch_set_session_name(agent, name).await,
        AgentCommand::Tree => dispatch_tree(agent).await,
        AgentCommand::DebugInfo => dispatch_debug_info(agent).await,
        AgentCommand::Share { theme } => dispatch_share(agent, theme).await,
        AgentCommand::BugReport {
            hint,
            include_session,
            include_summary,
        } => dispatch_bug_report(agent, hint, include_session, include_summary).await,
        AgentCommand::Export { out, theme } => dispatch_export(agent, out, theme).await,
        AgentCommand::Label { entry_id, label } => dispatch_label(agent, entry_id, label).await,
        AgentCommand::SwitchModel {
            provider,
            model_id,
            persist,
        } => dispatch_switch_model(agent, provider, model_id, persist).await,
        AgentCommand::SetThinking { level, persist } => {
            dispatch_set_thinking(agent, level, persist).await
        }
        AgentCommand::NewSession => dispatch_new_session(agent).await,
        AgentCommand::ResumeSession { path } => dispatch_resume_session(agent, path).await,
        AgentCommand::ApplyKey { provider, key } => dispatch_apply_key(agent, provider, key).await,
        AgentCommand::RebuildTools => dispatch_rebuild_tools(agent).await,
        AgentCommand::Reload {
            context_files,
            skills,
            system_prompt,
            append_system_prompt,
        } => {
            dispatch_reload(
                agent,
                context_files,
                skills,
                system_prompt,
                append_system_prompt,
            )
            .await
        }
        AgentCommand::ForkClone => dispatch_clone(agent).await,
        AgentCommand::ForkFrom { path, dir } => dispatch_fork_from(agent, path, dir).await,
        AgentCommand::ResumeZombie { path, action } => {
            dispatch_resume_zombie(agent, path, action).await
        }
        AgentCommand::SessionStats => dispatch_session_stats(agent).await,
        AgentCommand::Abort => None,
        AgentCommand::Shutdown => None,
    }
}

/// 轮次开始时已打开的 operation id 快照（用于中断后识别本轮新开的 operation）。
fn snapshot_open_operations(agent: &Agent) -> HashSet<String> {
    agent
        .session
        .as_ref()
        .map(|s| s.open_operation_ids().into_iter().collect())
        .unwrap_or_default()
}

/// 回合被硬中止（abort 抢先 → prompt future 被 drop）时收尾。
/// `run_loop` 的 operation_finished 与 future 一起被丢弃，这里为轮次中新开且仍未闭合的
/// operation 补写 interrupted；不补写则每次 Esc/Ctrl+C 都累积一个僵尸 operation。
fn close_aborted_operations(agent: &mut Agent, before: &HashSet<String>) {
    if let Some(s) = agent.session.as_mut() {
        _ = s.interrupt_operations_opened_since(before);
    }
}

/// 回合类命令（Prompt/PromptMessage/PromptBatch）共用的轮次骨架：
/// 锁 agent → 快照轮次开始前已打开的 operation → select 监听 abort oneshot。
/// abort 抢先时 prompt future 被 drop，`run_loop` 的 operation_finished 随之丢失，
/// 这里为轮次内新开且仍未闭合的 operation 补写 interrupted finish。
/// `f` 必须返回仅借用 `&mut Agent`（自带数据的 future）——HRTB 保证未来类型
/// 只与 guard 借用同生命周期，因此调用方需把参数 move 进闭包/`async move`。
async fn run_abortable_round<F>(agent: &Arc<Mutex<Agent>>, abort_rx: oneshot::Receiver<()>, f: F)
where
    F: for<'a> FnOnce(&'a mut Agent) -> BoxFuture<'a, Result<String>>,
{
    let mut g = agent.lock().await;
    let before = snapshot_open_operations(&g);
    let _res: Result<String> = tokio::select! {
        r = f(&mut g) => r,
        _ = abort_rx => {
            // 硬中止：置位本轮取消信号（持有 parent_abort 的扩展/工具据此联动取消），
            // 再换新信号并刷新扩展执行上下文（见 `abort_and_detach`）。
            g.abort_and_detach();

            core::extensions::dispatch_exec_ctx(&g.exec_ctx());
            close_aborted_operations(&mut g, &before);
            Err(Error::msg("aborted"))
        }
    };
}

/// 处理 AgentCommand::Prompt。
async fn dispatch_prompt(
    agent: &Arc<Mutex<Agent>>,
    text: String,
    pending_abort: &mut Option<AbortTx>,
) -> Option<RoundFuture> {
    let a = agent.clone();
    let (abort_tx, abort_rx) = oneshot::channel();
    *pending_abort = Some(abort_tx);
    Some(Box::pin(async move {
        run_abortable_round(&a, abort_rx, move |g| {
            Box::pin(async move { g.prompt(&text).await })
        })
        .await;
    }))
}

/// 处理 AgentCommand::PromptMessage。
async fn dispatch_prompt_message(
    agent: &Arc<Mutex<Agent>>,
    msg: AgentMessage,
    pending_abort: &mut Option<AbortTx>,
) -> Option<RoundFuture> {
    let a = agent.clone();
    let (abort_tx, abort_rx) = oneshot::channel();
    *pending_abort = Some(abort_tx);
    Some(Box::pin(async move {
        run_abortable_round(&a, abort_rx, move |g| Box::pin(g.prompt_message(msg))).await;
    }))
}

/// 处理 AgentCommand::PromptBatch。
async fn dispatch_prompt_batch(
    agent: &Arc<Mutex<Agent>>,
    msgs: Vec<AgentMessage>,
    pending_abort: &mut Option<AbortTx>,
) -> Option<RoundFuture> {
    let a = agent.clone();
    let (abort_tx, abort_rx) = oneshot::channel();
    *pending_abort = Some(abort_tx);
    Some(Box::pin(async move {
        run_abortable_round(&a, abort_rx, move |g| Box::pin(g.prompt_messages(msgs))).await;
    }))
}

/// 处理 AgentCommand::Compact。
async fn dispatch_compact(
    agent: &Arc<Mutex<Agent>>,
    instructions: Option<String>,
    pending_abort: &mut Option<AbortTx>,
) -> Option<RoundFuture> {
    let a = agent.clone();
    let (abort_tx, abort_rx) = oneshot::channel();
    *pending_abort = Some(abort_tx);
    Some(Box::pin(async move {
        let mut g = a.lock().await;
        let res = tokio::select! {
            r = g.maybe_compact_with(
                CompactionReason::Manual,
                instructions.as_deref(),
            ) => r,
            _ = abort_rx => Err(Error::msg("aborted")),
        };
        drop(g);

        // 回执：压缩成功携带 summary（tokensBefore / estimatedTokensAfter 供成功提示展示），
        // None = 跳过（Nothing to compact），Err = 失败。
        let payload = match &res {
            Ok(Some(summary)) => Ok(Some(summary.clone())),
            Ok(None) => Ok(None),
            Err(e) => Err(e.to_string()),
        };
        _ = run_in_event_loop(move |ui| ui.on_compact_done(payload));
    }))
}

/// 处理 AgentCommand::Navigate。
async fn dispatch_navigate(
    agent: &Arc<Mutex<Agent>>,
    target_id: String,
    summarize: bool,
    pending_abort: &mut Option<AbortTx>,
) -> Option<RoundFuture> {
    let a = agent.clone();
    let (abort_tx, abort_rx) = oneshot::channel();
    *pending_abort = Some(abort_tx);
    Some(Box::pin(async move {
        let mut g = a.lock().await;
        let res = tokio::select! {
            r = g.navigate_tree(&target_id, summarize) => r,
            _ = abort_rx => Err(crate::error::Error::msg("aborted")),
        };
        let payload = match &res {
            Ok((_changed, editor_text)) => Ok(NavigateDone {
                editor_text: editor_text.clone(),
                messages: g.messages.clone(),
            }),
            Err(e) => Err(e.to_string()),
        };
        drop(g);
        _ = run_in_event_loop(move |ui| ui.on_navigate_done(payload));
    }))
}

/// 处理 AgentCommand::Rewind：回退最后一次用户输入（活动分支叶子移到最后一条 user 消息之前）。
/// 非回合命令，空闲时立即执行；结果经 `on_rewind_done` 回执。
async fn dispatch_rewind(agent: &Arc<Mutex<Agent>>) -> Option<RoundFuture> {
    let mut a = agent.lock().await;
    match a.rewind_last_user_input() {
        Ok(removed_input) => {
            let messages = a.messages.clone();
            drop(a);
            _ = run_in_event_loop(move |ui| {
                ui.on_rewind_done(Ok(RewindDone {
                    removed_input,
                    messages,
                }))
            });
        }
        Err(e) => {
            drop(a);
            let err = e.to_string();
            _ = run_in_event_loop(move |ui| ui.on_rewind_done(Err(err)));
        }
    }
    None
}

/// 处理 AgentCommand::AppendMessage。
async fn dispatch_append_message(
    agent: &Arc<Mutex<Agent>>,
    msg: AgentMessage,
) -> Option<RoundFuture> {
    let mut a = agent.lock().await;
    a.messages.push(msg);
    None
}

/// 处理 AgentCommand::AppendCustomEntry：向当前会话追加自定义条目（扩展持久化）。
/// idle 执行（worker 仅在回合间隙处理命令），追加后经现有 sink 事件流回执
/// `extension:entry_persisted`（扩展按 customType 匹配消费）。无会话时回执 null。
async fn dispatch_append_custom_entry(
    agent: &Arc<Mutex<Agent>>,
    custom_type: String,
    data: Option<Value>,
) -> Option<RoundFuture> {
    let mut a = agent.lock().await;
    let id = a
        .session
        .as_mut()
        .map(|s| s.append_custom_entry(&custom_type, data))
        .unwrap_or_default();
    a.emit(json!({
        "type": "extension:entry_persisted",
        "customType": custom_type,
        "entryId": id
    }));
    None
}

/// 处理 AgentCommand::SetSessionName。
async fn dispatch_set_session_name(agent: &Arc<Mutex<Agent>>, name: String) -> Option<RoundFuture> {
    let mut a = agent.lock().await;
    let (ok, msg) = match a.session.as_mut() {
        Some(s) => {
            s.append_session_info(&name);
            (true, format!("session renamed: {}", name))
        }
        None => (false, "no current session".to_string()),
    };
    _ = run_in_event_loop(move |ui| ui.on_command_notice(ok, msg));
    None
}

/// 处理 AgentCommand::Tree。
async fn dispatch_tree(agent: &Arc<Mutex<Agent>>) -> Option<RoundFuture> {
    let a = agent.lock().await;
    let msg = match a.session.as_ref() {
        Some(s) => session_tree_text(s),
        None => "no current session".to_string(),
    };
    _ = run_in_event_loop(move |ui| ui.on_command_notice(true, msg));
    None
}

/// 处理 AgentCommand::DebugInfo。
async fn dispatch_debug_info(agent: &Arc<Mutex<Agent>>) -> Option<RoundFuture> {
    let a = agent.lock().await;
    let mut lines = vec![
        format!("provider: {}", a.model.provider),
        format!("model: {}", a.model.model_id),
        format!("api: {}", a.model.api),
        format!("thinking: {:?}", a.thinking_level),
        format!("messages: {}", a.messages.len()),
        format!("autoCompaction: {}", a.auto_compaction),
        format!("autoRetry: {}", a.auto_retry),
        format!("isCompacting: {}", a.is_compacting),
        format!("steerMode: {}", a.steer_mode),
        format!("followUpMode: {}", a.follow_up_mode),
    ];
    if let Some(s) = a.session.as_ref() {
        lines.push(format!("session: {}", s.session_id));
        lines.push(format!("leaf: {:?}", s.get_leaf_id()));
    }
    let msg = lines.join("\n");
    _ = run_in_event_loop(move |ui| ui.on_command_notice(true, msg));
    None
}

/// 处理 AgentCommand::Share。
async fn dispatch_share(agent: &Arc<Mutex<Agent>>, theme: Option<String>) -> Option<RoundFuture> {
    let a = agent.lock().await;
    let session = a.session.clone();
    let msg = match session {
        Some(session) => {
            let tmp = tempfile::Builder::new()
                .prefix(&format!("{APP_NAME}-share-"))
                .suffix(".html")
                .tempfile()
                .ok();
            match tmp {
                Some(tmp) => {
                    let path = tmp.path().to_string_lossy().to_string();
                    let session_file = session
                        .get_session_file()
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or_default();

                    match core::export_html::export_session_extra(
                        &session,
                        Some(&path),
                        theme.as_deref(),
                        Some(serde_json::json!({ "sessionFile": session_file })),
                    ) {
                        Ok(_) => {
                            // 直走 gh gist 私密上传
                            let out = std::process::Command::new("gh")
                                .args(["gist", "create", "--public=false", &path])
                                .output();

                            match out {
                                Ok(o) if o.status.success() => format!(
                                    "shared via gist: {}",
                                    String::from_utf8_lossy(&o.stdout).trim()
                                ),
                                Ok(o) => format!(
                                    "share failed: {}",
                                    String::from_utf8_lossy(&o.stderr).trim()
                                ),
                                Err(e) => format!("share failed: {}", e),
                            }
                        }
                        Err(e) => format!("share failed: {}", e),
                    }
                }
                None => "share failed: could not create temp file".to_string(),
            }
        }
        None => "share failed: no current session".to_string(),
    };

    let ok = !msg.starts_with("share failed");
    _ = run_in_event_loop(move |ui| ui.on_share_done(ok, msg));
    None
}

/// 处理 AgentCommand::BugReport：可选模型摘要 → 脱敏元数据 + 诊断 → 写 zip →
/// 会话内记一条自定义条目 + 清空已附带的崩溃日志，最后回执 UI。
async fn dispatch_bug_report(
    agent: &Arc<Mutex<Agent>>,
    hint: String,
    include_session: bool,
    include_summary: bool,
) -> Option<RoundFuture> {
    let mut a = agent.lock().await;
    let hint_opt = if hint.trim().is_empty() {
        None
    } else {
        Some(hint)
    };

    // 1) 可选：用当前模型把 transcript 概括成一份报告（不进会话历史）
    let mut summary: Option<String> = None;
    if include_summary {
        // 虚拟选择按 `direct` 理由路由后再做摘要（否则会把虚拟 api 发给 provider）。
        let model = a
            .summarization_model()
            .await
            .unwrap_or_else(|_| a.model.clone());

        let (system, prompt, max_tokens) =
            bug_report::build_summary_request(&a.messages, hint_opt.as_deref(), &model);

        match core::provider::simple_completion(&model, &system, &prompt, max_tokens).await {
            Ok((text, _)) if !text.trim().is_empty() => summary = Some(text),
            Ok(_) => {
                let msg =
                    "Failed to write bug report summary: model returned empty content".to_string();
                _ = run_in_event_loop(move |ui| ui.on_bug_report_done(false, msg));
                return None;
            }
            Err(e) => {
                let msg = format!("Failed to write bug report summary: {e}");
                _ = run_in_event_loop(move |ui| ui.on_bug_report_done(false, msg));
                return None;
            }
        }
    }

    // 2) 收集元数据 + 诊断（crashes 在打包前读取，避免清空后丢信息）
    let crashes = crash_log::read_crash_log();
    let has_crashes = !crashes.is_empty();
    let ctx = bug_report::BugReportContext {
        hint: hint_opt,
        session_id: a
            .session
            .as_ref()
            .map(|s| s.session_id.clone())
            .unwrap_or_default(),
        cwd: a.cwd.clone(),
        session_file: a
            .session
            .as_ref()
            .and_then(|s| s.get_session_file())
            .map(|p| p.to_string_lossy().to_string()),
        include_session,
        include_summary: summary.is_some(),
        message_count: a.messages.len(),
        model: &a.model,
        thinking_level: a.thinking_level.clone(),
    };
    let bundle = bug_report::build_bundle(bug_report::BundleInput {
        ctx,
        session: a.session.as_ref(),
        crashes: &crashes,
        summary,
    });
    let id = bundle
        .metadata
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    // 3) 写归档
    let path = PathBuf::from(&a.cwd).join(bug_report::archive_file_name(&id));
    if let Err(e) = bug_report::write_bug_report_archive(&bundle, &path) {
        let msg = format!("Failed to write bug report: {e}");
        _ = run_in_event_loop(move |ui| ui.on_bug_report_done(false, msg));
        return None;
    }

    // 4) 会话内记录（/bug 导出过什么，resume 后仍可追溯），并清空已附带的崩溃日志
    if let Some(s) = a.session.as_mut() {
        s.append_custom_entry(
            bug_report::BUG_REPORT_CUSTOM_ENTRY_TYPE,
            Some(json!({
                "id": id,
                "createdAt": now_iso(),
                "hint": bundle.metadata.get("hint").cloned().unwrap_or(Value::Null),
                "sessionIncluded": include_session,
                "summaryIncluded": bundle.summary.is_some(),
                "delivery": "zip",
                "path": path.to_string_lossy(),
            })),
        );
    }

    if has_crashes {
        crash_log::clear_crash_log();
    }

    let msg = format!(
        "Bug report exported to: {}\nReport ID: {}",
        path.display(),
        id
    );

    _ = run_in_event_loop(move |ui| ui.on_bug_report_done(true, msg));
    None
}

/// 处理 AgentCommand::Export。
async fn dispatch_export(
    agent: &Arc<Mutex<Agent>>,
    out: Option<String>,
    theme: Option<String>,
) -> Option<RoundFuture> {
    let a = agent.lock().await;
    let msg = match a.session.as_ref() {
        Some(s) => match core::export_html::export_session(s, out.as_deref(), theme.as_deref()) {
            Ok(p) => format!("exported: {}", p),
            Err(e) => format!("export failed: {}", e),
        },
        None => "no current session".to_string(),
    };
    let ok = msg.starts_with("exported");
    _ = run_in_event_loop(move |ui| ui.on_command_notice(ok, msg));
    None
}

/// 处理 AgentCommand::Label。
async fn dispatch_label(
    agent: &Arc<Mutex<Agent>>,
    entry_id: String,
    label: Option<String>,
) -> Option<RoundFuture> {
    let mut a = agent.lock().await;
    let msg = match a.session.as_mut() {
        Some(s) => match s.append_label_change(&entry_id, label.as_deref()) {
            Ok(_) => match label {
                Some(l) => format!("set label for {}: {}", entry_id, l),
                None => format!("cleared label for {}", entry_id),
            },
            Err(e) => format!("failed to set label: {}", e),
        },
        None => "no current session".to_string(),
    };
    let ok = !msg.starts_with("failed");
    _ = run_in_event_loop(move |ui| ui.on_command_notice(ok, msg));
    None
}

/// 组装模型状态回执载荷（/model 切换与 /login 自动改选共用）
fn model_changed_payload(
    a: &Agent,
    provider: &str,
    model_id: &str,
    persisted: bool,
) -> ModelChanged {
    ModelChanged {
        provider: a.model.provider.clone(),
        model: a.model.model_id.clone(),
        name: model_resolver::find_model(provider, model_id)
            .map(|e| e.name)
            .unwrap_or_else(|_| model_id.to_string()),
        reasoning: a.model.reasoning,
        thinking_level: a.thinking_level.clone(),
        thinking_levels: a
            .get_available_thinking_levels()
            .iter()
            .map(|s| s.to_string())
            .collect(),
        persisted,
    }
}

/// 处理 AgentCommand::SwitchModel。
async fn dispatch_switch_model(
    agent: &Arc<Mutex<Agent>>,
    provider: String,
    model_id: String,
    persist: bool,
) -> Option<RoundFuture> {
    let mut a = agent.lock().await;
    let payload = match a.switch_model(&provider, &model_id, persist) {
        Ok(()) => Ok(model_changed_payload(&a, &provider, &model_id, persist)),
        Err(e) => Err(e.to_string()),
    };
    _ = run_in_event_loop(move |ui| ui.on_model_changed(payload));
    None
}

/// 处理 AgentCommand::SetThinking：切换思考等级（`persist` 为真时写入设置），
/// 并把生效值与默认值回传 UI 事件循环；不启动新回合（恒返回 `None`）。
async fn dispatch_set_thinking(
    agent: &Arc<Mutex<Agent>>,
    level: String,
    persist: bool,
) -> Option<RoundFuture> {
    let mut a = agent.lock().await;
    let effective = a.set_thinking_level_silent(&level, persist);
    let default_level = if persist { Some(level) } else { None };
    _ = run_in_event_loop(move |ui| ui.on_thinking_level_changed(effective, default_level));
    None
}

/// 处理 AgentCommand::NewSession。
async fn dispatch_new_session(agent: &Arc<Mutex<Agent>>) -> Option<RoundFuture> {
    let mut a = agent.lock().await;
    let cwd = a.cwd.clone();
    let dir = session_manager::default_session_dir(&cwd, &settings_manager::agent_dir());
    _ = std::fs::create_dir_all(&dir);
    let payload = match session_manager::Session::create(&cwd, Some(dir), true) {
        Ok(s) => {
            let session_id = s.session_id.clone();
            let file = s
                .get_session_file()
                .map(|p| p.to_string_lossy().to_string());

            a.messages.clear();
            a.session = Some(s);

            // 会话切换后刷新扩展的执行上下文（避免继续用旧会话的模板/cwd）
            core::extensions::dispatch_exec_ctx(&a.exec_ctx());

            Ok(SessionSwitched {
                session_id,
                name: None,
                file,
                messages: Vec::new(),
            })
        }
        Err(e) => Err(e.to_string()),
    };
    _ = run_in_event_loop(move |ui| ui.on_session_switched(payload));
    None
}

/// 把打开的会话装配进 agent：构建消息上下文、设置当前 session，返回切换回执。
fn attach_session(a: &mut Agent, s: Session) -> std::result::Result<SessionSwitched, String> {
    a.messages = s.build_context_messages();
    let file = s
        .get_session_file()
        .map(|p| p.to_string_lossy().to_string());
    let id = s.session_id.clone();
    let name = s.name.clone();
    a.session = Some(s);

    // 会话切换后刷新扩展的执行上下文（避免继续用旧会话的模板/cwd）
    core::extensions::dispatch_exec_ctx(&a.exec_ctx());

    Ok(SessionSwitched {
        session_id: id,
        name,
        file,
        messages: a.messages.clone(),
    })
}

/// 处理 AgentCommand::ResumeSession。
/// 中间行损坏的可修复会话不直接报错，而是弹修复确认面板（截断后重开）；
/// 存在 ≥2 个崩溃残留的未完成 operation（僵尸）时**不加载**会话，先弹选择面板，
/// 用户确认恢复方式（Continue / Rewrite）后才经 ResumeZombie 加载。
async fn dispatch_resume_session(agent: &Arc<Mutex<Agent>>, path: String) -> Option<RoundFuture> {
    let mut a = agent.lock().await;
    let payload = match session_manager::Session::open_checked(&path) {
        OpenSessionResult::Ok(s) => {
            let zombie_count = s.open_operations_count();
            if zombie_count >= 2 {
                // 存在崩溃残留：先不加载文件，弹面板让用户选择恢复方式
                _ = run_in_event_loop(move |ui| {
                    ui.stage_zombie_operations(ZombieOperationsRequest {
                        path: path.clone().into(),
                        count: zombie_count,
                    });
                });
                return None;
            }
            attach_session(&mut a, *s)
        }
        OpenSessionResult::Invalid(e) => Err(e.to_string()),
        OpenSessionResult::Repairable { path, plan } => {
            // 可修复：弹确认面板（不切换会话；确认后 Repair 重发 ResumeSession）
            _ = run_in_event_loop(move |ui| {
                ui.stage_session_repair(SessionRepairRequest {
                    path,
                    plan,
                    action: RepairAction::Resume,
                });
            });
            return None;
        }
    };
    _ = run_in_event_loop(move |ui| ui.on_session_switched(payload));
    None
}

/// 处理 AgentCommand::ResumeZombie：僵尸操作面板确认后的恢复命令。
/// 面板选择前会话未加载（见 dispatch_resume_session）；
/// action=continue → 只读容忍装配；action=rewrite → 先压平（为旧 operation 补
/// `operation_finished(outcome="interrupted")`）再装配。确认路径不再二次弹僵尸面板。
async fn dispatch_resume_zombie(
    agent: &Arc<Mutex<Agent>>,
    path: String,
    action: String,
) -> Option<RoundFuture> {
    let mut a = agent.lock().await;
    let mut s = match session_manager::Session::open_checked(&path) {
        OpenSessionResult::Ok(s) => *s,
        OpenSessionResult::Invalid(e) => {
            _ = run_in_event_loop(move |ui| {
                ui.push_msg(format!("Failed to open session: {e}"), MsgLevel::Error);
            });
            return None;
        }
        OpenSessionResult::Repairable { path, plan } => {
            // 打开时反而行损坏（防御）：走既有修复面板
            _ = run_in_event_loop(move |ui| {
                ui.stage_session_repair(SessionRepairRequest {
                    path,
                    plan,
                    action: RepairAction::Resume,
                });
            });
            return None;
        }
    };

    // rewrite：先压平再加载；continue：直接加载（只读容忍）
    let squash_msg: Option<String> = if action == "rewrite" {
        let n = s.squash_stale_operations();
        match s.take_persist_error() {
            Some(err) => Some(format!(
                "Squashed {n} interrupted run(s), but the disk write failed: {err}"
            )),
            None if n == 0 => Some("No stale operations to squash".to_string()),
            None => Some(format!(
                "Marked {n} interrupted run(s) as finished (session log updated)"
            )),
        }
    } else {
        None
    };

    let payload = attach_session(&mut a, s);
    _ = run_in_event_loop(move |ui| {
        if let Some(msg) = squash_msg {
            ui.push_msg(msg, MsgLevel::Info);
        }
        ui.on_session_switched(payload);
    });
    None
}

/// 处理 AgentCommand::ApplyKey。
async fn dispatch_apply_key(
    agent: &Arc<Mutex<Agent>>,
    provider: String,
    key: String,
) -> Option<RoundFuture> {
    let mut a = agent.lock().await;
    if a.model.provider == provider {
        a.model.api_key = key.clone();
    }

    // fallback 模型时自动选择该 provider 的默认模型
    let mut applied = None;
    if a.model_is_fallback
        && let Some((p, m)) = model_resolver::default_model_for(&provider)
        && a.switch_model(&p, &m, true).is_ok()
    {
        applied = Some(model_changed_payload(&a, &p, &m, true));
    }

    _ = run_in_event_loop(move |ui| ui.on_applied_key(AppliedKey { applied }));
    None
}

/// 处理 AgentCommand::RebuildTools。
async fn dispatch_rebuild_tools(agent: &Arc<Mutex<Agent>>) -> Option<RoundFuture> {
    let mut a = agent.lock().await;
    a.rebuild_tools();
    _ = run_in_event_loop(|ui| ui.on_tools_rebuilt());
    None
}

/// 处理 AgentCommand::Reload（/reload 与项目信任确认）。
///
/// compose_tools 消费的是 `agent.skills` + `agent.rebuild_ctx`（worker 侧缓存），
/// UI 侧副本只用于斜杠命令展开；重载后必须把新值写入 worker，否则系统提示仍用启动时的旧快照。
async fn dispatch_reload(
    agent: &Arc<Mutex<Agent>>,
    context_files: Vec<(String, String)>,
    skills: Vec<Skill>,
    system_prompt: Option<String>,
    append_system_prompt: Option<String>,
) -> Option<RoundFuture> {
    let mut a = agent.lock().await;
    a.skills = skills;
    a.rebuild_ctx.context_files = context_files;
    a.rebuild_ctx.system_prompt = system_prompt;
    a.rebuild_ctx.append_system_prompt = append_system_prompt;
    a.reload_default_tools();
    a.rebuild_tools();
    None
}

/// 处理 AgentCommand::ForkClone（/clone）。
async fn dispatch_clone(agent: &Arc<Mutex<Agent>>) -> Option<RoundFuture> {
    let mut a = agent.lock().await;
    let cwd = a.cwd.clone();
    let source = a
        .session
        .as_ref()
        .and_then(|s| s.get_session_file())
        .map(|p| p.to_string_lossy().to_string());
    let leaf = a.session.as_ref().and_then(|s| s.clone_target_id());

    let payload = match (source, leaf) {
        (Some(p), Some(lid)) => apply_fork_result(
            &mut a,
            "clone",
            session_manager::Session::fork_at(&p, &lid, &cwd, None),
        ),
        (Some(_), None) => Err("clone failed: no leaf to clone at".to_string()),
        (None, _) => Err("no current session".to_string()),
    };

    deliver_fork_payload(payload);
    None
}

/// 处理 AgentCommand::ForkFrom（/fork 与修复面板共用）。
/// /fork 传当前会话路径（fork_from 整树复制）；修复确认时源文件已在面板截断修复。
async fn dispatch_fork_from(
    agent: &Arc<Mutex<Agent>>,
    path: String,
    dir: Option<PathBuf>,
) -> Option<RoundFuture> {
    let mut a = agent.lock().await;
    let cwd = a.cwd.clone();
    let payload = apply_fork_result(
        &mut a,
        "fork",
        session_manager::Session::fork_from(&path, &cwd, dir),
    );
    deliver_fork_payload(payload);
    None
}

/// fork/clone 成功结果写进 agent 并组装回执
fn apply_fork_result(
    a: &mut Agent,
    op: &str,
    result: Result<Session>,
) -> std::result::Result<(String, SessionSwitched), String> {
    match result {
        Ok(s) => {
            let file = s
                .get_session_file()
                .map(|p| p.to_string_lossy().to_string());
            let id = s.session_id.clone();
            let name = s.name.clone();
            a.messages = s.build_context_messages();
            let messages = a.messages.clone();
            a.session = Some(s);

            // 恢复出来的历史里用过的延迟工具重新激活，并重建工具表（否则模型下一轮就用不到它）
            if core::extensions::restore_deferred_activations(&messages) {
                _ = core::extensions::take_deferred_activation_dirty();
                a.rebuild_tools();
            }

            Ok((
                format!(
                    "created new session via {}: {}",
                    op,
                    file.clone().unwrap_or_default()
                ),
                SessionSwitched {
                    session_id: id,
                    name,
                    file,
                    messages,
                },
            ))
        }
        Err(e) => Err(format!("{} failed: {}", op, e)),
    }
}

/// fork/clone 回执到 UI：先切换会话（on_session_switched 会清旧系统提示），再报成功，避免消息被清掉
fn deliver_fork_payload(payload: std::result::Result<(String, SessionSwitched), String>) {
    _ = run_in_event_loop(move |ui| match payload {
        Ok((note, switched)) => {
            ui.on_session_switched(Ok(switched));
            ui.push_msg(note, MsgLevel::Success);
        }
        Err(e) => ui.on_command_notice(false, e),
    });
}

/// 处理 AgentCommand::SessionStats。
async fn dispatch_session_stats(agent: &Arc<Mutex<Agent>>) -> Option<RoundFuture> {
    let a = agent.lock().await;
    let stats = session_stats(&a);
    _ = run_in_event_loop(move |ui| ui.on_session_stats(stats));
    None
}

/// /session 统计（worker 侧权威：messages + session entries usage）
/// 累计单条消息的角色计数与 usage（输入/输出/缓存 token、成本）
#[allow(clippy::too_many_arguments)]
fn accumulate_message_usage(
    m: &AgentMessage,
    user: &mut usize,
    assistant: &mut usize,
    tool_results: &mut usize,
    tool_calls: &mut usize,
    input: &mut u64,
    output: &mut u64,
    cache_read: &mut u64,
    cache_write: &mut u64,
    cost: &mut f64,
) {
    match m.role.as_str() {
        "user" => *user += 1,
        "assistant" => {
            *assistant += 1;
            for c in &m.content {
                if matches!(c, ContentBlock::ToolCall { .. }) {
                    *tool_calls += 1;
                }
            }
        }
        "toolResult" => *tool_results += 1,
        _ => {}
    }

    // 摘要类消息（投影带上条目 usage 以便 footer 统计）计入 [`accumulate_session_usage`]，这里跳过以免重复累加
    if matches!(m.role.as_str(), "compactionSummary" | "branchSummary") {
        return;
    }

    if let Some(u) = &m.usage {
        *input += u.input as u64;
        *output += u.output as u64;
        *cache_read += u.cache_read as u64;
        *cache_write += u.cache_write as u64;
        *cost += u.cost.total;
    }
}

/// 累计会话条目/记录里的 usage：
/// - `compaction` / `branch_summary`：生成摘要那一次调用的开销；
/// - `usage_record`：不经会话回合的用量（目前只有 `cause = cache_warm` 的保温请求）——
///   它不会落成 assistant 消息，不计入就等于把这部分成本从会话成本里漏掉。
fn accumulate_session_usage(
    entries: &[Value],
    input: &mut u64,
    output: &mut u64,
    cache_read: &mut u64,
    cache_write: &mut u64,
    cost: &mut f64,
) {
    for e in entries {
        let ty = e.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if !matches!(ty, "compaction" | "branch_summary" | "usage_record") {
            continue;
        }

        if let Some(u) = e.get("usage")
            && let Ok(usage) = serde_json::from_value::<Usage>(u.clone())
        {
            *input += usage.input as u64;
            *output += usage.output as u64;
            *cache_read += usage.cache_read as u64;
            *cache_write += usage.cache_write as u64;
            *cost += usage.cost.total;
        }
    }
}

/// 汇总会话统计：消息计数、token 用量与成本（当前 messages 与磁盘会话条目合计）。
fn session_stats(a: &Agent) -> SessionStats {
    let mut user = 0usize;
    let mut assistant = 0usize;
    let mut tool_results = 0usize;
    let mut tool_calls = 0usize;
    let mut input: u64 = 0;
    let mut output: u64 = 0;
    let mut cache_read: u64 = 0;
    let mut cache_write: u64 = 0;
    let mut cost: f64 = 0.0;

    for m in &a.messages {
        accumulate_message_usage(
            m,
            &mut user,
            &mut assistant,
            &mut tool_results,
            &mut tool_calls,
            &mut input,
            &mut output,
            &mut cache_read,
            &mut cache_write,
            &mut cost,
        );
    }
    if let Some(s) = a.session.as_ref() {
        accumulate_session_usage(
            &s.get_entries(),
            &mut input,
            &mut output,
            &mut cache_read,
            &mut cache_write,
            &mut cost,
        );
    }

    let total_messages = user + assistant + tool_results;
    let prompt_tokens = input + cache_read + cache_write;
    let total_tokens = input + output + cache_read + cache_write;
    let cost_breakdown = session_cost_breakdown(a);

    let file = a
        .session
        .as_ref()
        .and_then(|s| s.get_session_file())
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| "In-memory".to_string());
    let id = a
        .session
        .as_ref()
        .map(|s| s.session_id.clone())
        .unwrap_or_else(|| "—".to_string());
    let name = a.session.as_ref().and_then(|s| s.name.clone());

    SessionStats {
        name,
        file,
        id,
        messages_total: total_messages as u64,
        tool_calls,
        prompt_tokens,
        total_tokens,
        input,
        output,
        cache_read,
        cache_write,
        cost_total: cost,
        cost_breakdown,
        cache_warming: a.cache_warmer.status(),
    }
}

/// 按 `provider/model` 归集成本
///
/// 数据源与 [`session_stats`] 的总成本**逐一对齐**（消息 + 会话条目的摘要），
/// 以保证分列之和等于 `Cost Total`。
fn session_cost_breakdown(a: &Agent) -> Vec<CostBreakdownEntry> {
    let entries = a
        .session
        .as_ref()
        .map(|s| s.get_entries())
        .unwrap_or_default();
    usage_cost_breakdown(&a.messages, &entries)
}

/// 成本分列：
/// - assistant 消息按 `provider/responseModel||model` 分组（网关 `auto` 之类会解析成具体模型）；
/// - toolResult（嵌套调用）与 compaction/branch_summary（摘要生成）没有模型归属，统一进 `Tools/summaries` 桶。
fn usage_cost_breakdown(messages: &[AgentMessage], entries: &[Value]) -> Vec<CostBreakdownEntry> {
    /// 把一条 usage 累加进 totals：已有同 key 条目则累加，否则新建。
    fn add(totals: &mut Vec<CostBreakdownEntry>, key: String, usage: &Usage) {
        let tokens = (usage.input + usage.output + usage.cache_read + usage.cache_write) as u64;
        match totals.iter_mut().find(|e| e.key == key) {
            Some(e) => {
                e.cost += usage.cost.total;
                e.tokens += tokens;
            }
            None => totals.push(CostBreakdownEntry {
                key,
                cost: usage.cost.total,
                tokens,
            }),
        }
    }

    let mut totals: Vec<CostBreakdownEntry> = Vec::new();
    for m in messages {
        let Some(u) = &m.usage else { continue };
        match m.role.as_str() {
            "assistant" => {
                let provider = m.provider.as_deref().unwrap_or("unknown");
                let model = m
                    .response_model
                    .as_deref()
                    .or(m.model.as_deref())
                    .unwrap_or("unknown");
                add(&mut totals, format!("{provider}/{model}"), u);
            }
            "toolResult" => add(&mut totals, TOOLS_BUCKET.to_string(), u),
            _ => {}
        }
    }

    for e in entries {
        let ty = e.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if !matches!(ty, "compaction" | "branch_summary" | "usage_record") {
            continue;
        }

        let Some(u) = e.get("usage") else { continue };
        let Ok(usage) = serde_json::from_value::<Usage>(u.clone()) else {
            continue;
        };

        // 用量记录（缓存保温）带 provider/model 归属时按模型归集，
        // 其余（摘要、无归属的旧记录）进 Tools 桶
        let key = match (
            e.get("provider").and_then(|v| v.as_str()),
            e.get("model").and_then(|v| v.as_str()),
        ) {
            (Some(provider), Some(model)) if ty == "usage_record" => {
                format!("{provider}/{model}")
            }
            _ => TOOLS_BUCKET.to_string(),
        };
        add(&mut totals, key, &usage);
    }

    let mut out: Vec<CostBreakdownEntry> = totals
        .into_iter()
        .filter(|e| e.cost > 0.0 || e.tokens > 0)
        .collect();
    out.sort_by(|a, b| b.cost.total_cmp(&a.cost));
    out
}

/// /tree 会话树文本
fn session_tree_text(s: &Session) -> String {
    let tree = s.get_tree();
    let mut lines = Vec::new();

    /// 递归把会话树节点按 depth 缩进写入 lines；`budget` 为剩余可输出行数，
    /// 用尽时以省略号收尾。
    fn walk(nodes: &[Value], depth: usize, lines: &mut Vec<String>, budget: &mut usize) {
        for node in nodes {
            if *budget == 0 {
                lines.push(format!("{}...", "  ".repeat(depth)));
                return;
            }
            *budget -= 1;
            let entry = &node["entry"];
            let ty = entry.get("type").and_then(|v| v.as_str()).unwrap_or("?");
            let label = match ty {
                "message" => format!(
                    "msg/{}",
                    entry
                        .get("message")
                        .and_then(|m| m.get("role"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("?")
                ),
                "model_change" => "model".to_string(),
                "compaction" => "compact".to_string(),
                "branch_summary" => "branch_summary".to_string(),
                // append-only 投影指令：不进模型上下文，仅在树里标记
                "context_edit" => {
                    let verb = if entry.get("replacement").is_none_or(|v| v.is_null()) {
                        "omit"
                    } else {
                        "replace"
                    };
                    format!(
                        "ctx_{}/{}",
                        verb,
                        entry.get("targetId").and_then(|v| v.as_str()).unwrap_or("")
                    )
                }
                "label" => entry
                    .get("label")
                    .and_then(|v| v.as_str())
                    .unwrap_or("label")
                    .to_string(),
                "session_info" => "name".to_string(),
                "thinking_level_change" => "thinking".to_string(),
                other => other.to_string(),
            };
            let id = entry.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let user_label = node.get("label").and_then(|v| v.as_str());
            match user_label {
                Some(l) => lines.push(format!("{}{} [{}] {}", "  ".repeat(depth), label, l, id)),
                None => lines.push(format!("{}{} {}", "  ".repeat(depth), label, id)),
            }
            if let Some(children) = node.get("children").and_then(|v| v.as_array()) {
                walk(children, depth + 1, lines, budget);
            }
        }
    }

    let mut budget = 40usize;
    walk(
        tree.get("tree")
            .and_then(|v| v.as_array())
            .map(|a| a.as_slice())
            .unwrap_or(&[]),
        0,
        &mut lines,
        &mut budget,
    );
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::arc_with_non_send_sync)]
    use super::super::agent_actor::{AgentCommand, CommandTx, WorkerHandle, channels};
    use super::super::{UiTask, clear_event_loop_tx, set_event_loop_tx};
    use super::*;
    use crate::core::agent_session::ToolSelection;
    use crate::core::session_manager::list_sessions;
    use crate::modes::interactive::app::{App, sys_msg_text};
    use crate::utils::time::now_ms;
    use std::time::Duration;
    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

    fn test_agent_arc() -> Arc<Mutex<Agent>> {
        let dir = std::env::temp_dir().join("prux-worker-tests");
        let _ = std::fs::create_dir_all(&dir);
        unsafe { std::env::set_var("PRUX_AGENT_DIR", &dir) };
        let entry = crate::core::model_resolver::find_model("deepseek", "deepseek-flash").unwrap();
        let a = Agent::new(
            entry,
            "/tmp".to_string(),
            ToolSelection::NoBuiltinTools,
            Vec::new(),
            None,
            None,
            None,
            Vec::new(),
            None,
            None,
            false,
            Vec::new(),
        )
        .unwrap();
        Arc::new(tokio::sync::Mutex::new(a))
    }

    /// 启动 worker 并注册全局 UI 队列；返回 (worker handle, UI 队列接收端)。
    fn start_worker(cmd_rx: CommandRx) -> (tokio::task::JoinHandle<()>, UnboundedReceiver<UiTask>) {
        let (ui_tx, ui_rx) = unbounded_channel::<UiTask>();
        set_event_loop_tx(ui_tx);
        let handle = tokio::spawn(async move {
            run_worker(test_agent_arc(), cmd_rx).await;
        });
        (handle, ui_rx)
    }

    /// 用指定 agent 启动 worker（fork/clone 等依赖真实会话的命令用）
    fn start_worker_with_agent(
        agent: Arc<Mutex<Agent>>,
        cmd_rx: CommandRx,
    ) -> (tokio::task::JoinHandle<()>, UnboundedReceiver<UiTask>) {
        let (ui_tx, ui_rx) = unbounded_channel::<UiTask>();
        set_event_loop_tx(ui_tx);
        let handle = tokio::spawn(async move {
            run_worker(agent, cmd_rx).await;
        });
        (handle, ui_rx)
    }

    /// 依次应用 UI 队列闭包到 app，直到 `pred` 满足或超时。
    async fn apply_until(
        rx: &mut UnboundedReceiver<UiTask>,
        app: &mut App,
        mut pred: impl FnMut(&App) -> bool,
    ) -> std::result::Result<(), String> {
        for _ in 0..64 {
            let task = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .map_err(|_| "timeout waiting for ui task".to_string())?
                .ok_or("ui queue closed".to_string())?;
            task(app);
            if pred(app) {
                return Ok(());
            }
        }
        Err("did not observe expected state".to_string())
    }

    /// 让 worker 干净退出（drop 全部 cmd_tx → cmd_rx None → 循环 break）。
    async fn shutdown(
        worker: WorkerHandle,
        cmd_tx: CommandTx,
        handle: tokio::task::JoinHandle<()>,
    ) {
        clear_event_loop_tx();
        worker.abort();
        drop(worker);
        drop(cmd_tx);
        let _ = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("worker 应干净退出");
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn idle_commands_reply_through_ui_queue() {
        // 与 theme/auth 等测试共用进程级 PRUX_AGENT_DIR：串行化避免污染
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (cmd_tx, cmd_rx) = channels();
        let worker = WorkerHandle::new(cmd_tx.clone());
        let (handle, mut ui_rx) = start_worker(cmd_rx);

        worker.send(AgentCommand::RebuildTools);
        let mut app = App::new();
        apply_until(&mut ui_rx, &mut app, |a| {
            a.system_messages
                .iter()
                .any(|(_, m)| sys_msg_text(m).contains("tools rebuilt"))
        })
        .await
        .expect("RebuildTools 应回执提示");
        shutdown(worker, cmd_tx, handle).await;
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn switch_model_replies_with_new_model_state() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (cmd_tx, cmd_rx) = channels();
        let worker = WorkerHandle::new(cmd_tx.clone());
        let (handle, mut ui_rx) = start_worker(cmd_rx);

        worker.send(AgentCommand::SwitchModel {
            provider: "deepseek".into(),
            model_id: "deepseek-v4-pro".into(),
            persist: false,
        });
        let mut app = App::new();
        apply_until(&mut ui_rx, &mut app, |a| {
            a.current_model.as_deref() == Some("deepseek-v4-pro")
        })
        .await
        .expect("应有 model_changed 回执更新 current_model");
        shutdown(worker, cmd_tx, handle).await;
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn switch_model_persists_default_only_when_requested() {
        // 对齐 pi：setModel 只在 persist=true 时写默认；persist=false 的普通切换不写盘
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join("prux-worker-tests");
        let _ = std::fs::remove_file(dir.join("settings.json"));
        let (cmd_tx, cmd_rx) = channels();
        let worker = WorkerHandle::new(cmd_tx.clone());
        let (handle, mut ui_rx) = start_worker(cmd_rx);

        // persist=false：只切换，不写默认
        worker.send(AgentCommand::SwitchModel {
            provider: "deepseek".into(),
            model_id: "deepseek-v4-pro".into(),
            persist: false,
        });
        let mut app = App::new();
        apply_until(&mut ui_rx, &mut app, |a| {
            a.current_model.as_deref() == Some("deepseek-v4-pro")
        })
        .await
        .expect("应有模型切换回执");
        assert!(
            crate::core::settings_manager::read_settings()
                .default_model
                .is_none(),
            "persist=false 不应写默认模型"
        );

        // persist=true：切换成功后才写默认，App 缓存与状态栏同步
        worker.send(AgentCommand::SwitchModel {
            provider: "deepseek".into(),
            model_id: "deepseek-flash".into(),
            persist: true,
        });
        apply_until(&mut ui_rx, &mut app, |a| {
            a.default_model.as_deref() == Some("deepseek-flash")
        })
        .await
        .expect("persist=true 应更新 App 默认缓存");
        let s = crate::core::settings_manager::read_settings();
        assert_eq!(s.default_provider.as_deref(), Some("deepseek"));
        assert_eq!(s.default_model.as_deref(), Some("deepseek-flash"));
        assert!(
            app.status.contains("Default model"),
            "状态栏应有默认模型提示: {:?}",
            app.status
        );

        shutdown(worker, cmd_tx, handle).await;
        let _ = std::fs::remove_file(dir.join("settings.json"));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn set_thinking_persists_default_only_when_requested() {
        // 对齐 pi：setThinkingLevel 只在 persist=true 时写默认
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join("prux-worker-tests");
        let _ = std::fs::remove_file(dir.join("settings.json"));
        let (cmd_tx, cmd_rx) = channels();
        let worker = WorkerHandle::new(cmd_tx.clone());
        let (handle, mut ui_rx) = start_worker(cmd_rx);

        // persist=false：只设置，不写默认
        worker.send(AgentCommand::SetThinking {
            level: "high".into(),
            persist: false,
        });
        let mut app = App::new();
        apply_until(&mut ui_rx, &mut app, |a| a.thinking_level.is_some())
            .await
            .expect("应有 thinking 回执");
        assert!(
            crate::core::settings_manager::read_settings()
                .default_thinking_level
                .is_none(),
            "persist=false 不应写默认级别"
        );

        // persist=true：写默认 + App 缓存 + 状态栏
        worker.send(AgentCommand::SetThinking {
            level: "high".into(),
            persist: true,
        });
        apply_until(&mut ui_rx, &mut app, |a| {
            a.default_thinking_level.as_deref() == Some("high")
        })
        .await
        .expect("persist=true 应更新 App 默认级别缓存");
        assert_eq!(
            crate::core::settings_manager::read_settings()
                .default_thinking_level
                .as_deref(),
            Some("high")
        );
        assert!(
            app.status.contains("Default thinking level"),
            "状态栏应有默认级别提示: {:?}",
            app.status
        );

        shutdown(worker, cmd_tx, handle).await;
        let _ = std::fs::remove_file(dir.join("settings.json"));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn apply_key_auto_select_updates_ui_model_mirror() {
        // 回归：首次启动无配置（兜底模型）→ /login 自动改选默认模型后，UI 镜像必须同步，
        // 否则 footer 隐藏模型名、/model 面板不勾选当前模型
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (cmd_tx, cmd_rx) = channels();
        let worker = WorkerHandle::new(cmd_tx.clone());
        let agent = test_agent_arc();
        agent.lock().await.model_is_fallback = true;
        let probe = agent.clone();
        let (handle, mut ui_rx) = start_worker_with_agent(agent, cmd_rx);

        // init_app 首帧：兜底 provider 无凭据 → no_model_available
        let mut app = App::new();
        app.no_model_available = true;
        app.current_model = Some("deepseek-flash".to_string());
        app.current_provider = Some("deepseek".to_string());

        worker.send(AgentCommand::ApplyKey {
            provider: "anthropic".into(),
            key: "sk-test".into(),
        });
        apply_until(&mut ui_rx, &mut app, |a| {
            a.current_provider.as_deref() == Some("anthropic")
        })
        .await
        .expect("登录后应同步 current_provider 镜像");
        assert_eq!(app.current_model.as_deref(), Some("claude-opus-4-8"));
        assert!(!app.no_model_available, "footer 应恢复显示模型名");
        assert!(
            app.system_messages
                .iter()
                .any(|(_, m)| sys_msg_text(m).contains("model selected for the signed-in")),
            "应有模型自动选中提示"
        );
        assert!(
            !probe.lock().await.model_is_fallback,
            "落地模型后不再是兜底态，再次 /login 不得改选"
        );
        shutdown(worker, cmd_tx, handle).await;
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn set_session_name_and_tree_reply_via_command_notice() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (cmd_tx, cmd_rx) = channels();
        let worker = WorkerHandle::new(cmd_tx.clone());
        let (handle, mut ui_rx) = start_worker(cmd_rx);
        let mut app = App::new();

        // SetSessionName：无会话时回 notice 失败消息
        worker.send(AgentCommand::SetSessionName {
            name: "demo".into(),
        });
        apply_until(&mut ui_rx, &mut app, |a| {
            a.system_messages
                .iter()
                .any(|(_, m)| sys_msg_text(m).contains("no current session"))
        })
        .await
        .expect("SetSessionName 无会话应回 notice 失败");

        // Tree：无会话时回 notice（no current session）
        worker.send(AgentCommand::Tree);
        apply_until(&mut ui_rx, &mut app, |a| {
            a.system_messages
                .iter()
                .any(|(_, m)| sys_msg_text(m).contains("no current session"))
        })
        .await
        .expect("Tree 无会话应回 notice");
        shutdown(worker, cmd_tx, handle).await;
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn export_and_debug_reply_via_command_notice() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (cmd_tx, cmd_rx) = channels();
        let worker = WorkerHandle::new(cmd_tx.clone());
        let (handle, mut ui_rx) = start_worker(cmd_rx);
        let mut app = App::new();

        worker.send(AgentCommand::DebugInfo);
        apply_until(&mut ui_rx, &mut app, |a| {
            a.system_messages
                .iter()
                .any(|(_, m)| sys_msg_text(m).contains("provider"))
        })
        .await
        .expect("DebugInfo 应回 notice 含 provider");

        worker.send(AgentCommand::Export {
            out: None,
            theme: None,
        });
        apply_until(&mut ui_rx, &mut app, |a| {
            a.system_messages
                .iter()
                .any(|(_, m)| sys_msg_text(m).contains("no current session"))
        })
        .await
        .expect("Export 无会话应回 notice 失败");
        shutdown(worker, cmd_tx, handle).await;
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn export_uses_current_theme() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join("prux-worker-export-theme");
        let themes = dir.join("themes");
        let _ = std::fs::create_dir_all(&themes);
        std::fs::write(
            themes.join("mytheme.json"),
            r##"{"name":"mytheme","vars":{"fg":"#abcdef"},"colors":{"text":"fg"},"export":{"pageBg":"#123456","cardBg":"#234567","infoBg":"#345678"}}"##,
        )
        .unwrap();
        unsafe { std::env::set_var("PRUX_AGENT_DIR", &dir) };
        let entry = crate::core::model_resolver::find_model("deepseek", "deepseek-flash").unwrap();
        let session = session_manager::Session::create("/tmp", Some(dir.clone()), true).unwrap();
        let agent = Arc::new(tokio::sync::Mutex::new(
            Agent::new(
                entry,
                "/tmp".to_string(),
                ToolSelection::NoBuiltinTools,
                Vec::new(),
                None,
                None,
                None,
                Vec::new(),
                Some(session),
                None,
                false,
                Vec::new(),
            )
            .unwrap(),
        ));

        let (cmd_tx, cmd_rx) = channels();
        let worker = WorkerHandle::new(cmd_tx.clone());
        let (handle, mut ui_rx) = start_worker_with_agent(agent, cmd_rx);
        let mut app = App::new();

        let out = dir.join("out.html");
        worker.send(AgentCommand::Export {
            out: Some(out.to_string_lossy().to_string()),
            theme: Some("mytheme".to_string()),
        });
        apply_until(&mut ui_rx, &mut app, |a| {
            a.system_messages
                .iter()
                .any(|(_, m)| sys_msg_text(m).contains("exported:"))
        })
        .await
        .expect("Export 应回 notice 成功");

        let html = std::fs::read_to_string(&out).unwrap();
        assert!(
            html.contains("--exportPageBg: #123456;"),
            "导出应使用当前主题而非 dark"
        );
        assert!(html.contains("--text: #abcdef;"), "导出应含主题文本色");
        shutdown(worker, cmd_tx, handle).await;
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(themes.join("mytheme.json"));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn busy_commands_are_queued_and_executed_after_round() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (cmd_tx, cmd_rx) = channels();
        let worker = WorkerHandle::new(cmd_tx.clone());
        let (handle, mut ui_rx) = start_worker(cmd_rx);
        let mut app = App::new();

        // 发起回合（无凭据快速失败）同时立刻发 idle 命令 → 排队
        worker.prompt("will fail fast".into());
        worker.send(AgentCommand::RebuildTools);
        apply_until(&mut ui_rx, &mut app, |a| {
            a.system_messages
                .iter()
                .any(|(_, m)| sys_msg_text(m).contains("tools rebuilt"))
        })
        .await
        .expect("回合结束后的排队命令应被执行");
        shutdown(worker, cmd_tx, handle).await;
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn fork_clone_without_session_errors() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (cmd_tx, cmd_rx) = channels();
        let worker = WorkerHandle::new(cmd_tx.clone());
        let (handle, mut ui_rx) = start_worker(cmd_rx);
        let mut app = App::new();

        // /fork 走 ForkFrom：无有效源时 worker 报 fork failed，而非 panic
        worker.send(AgentCommand::ForkFrom {
            path: "/nonexistent/no-session.jsonl".into(),
            dir: None,
        });
        apply_until(&mut ui_rx, &mut app, |a| {
            a.system_messages
                .iter()
                .any(|(_, m)| sys_msg_text(m).contains("fork failed"))
        })
        .await
        .expect("fork 无有效源应回 notice 失败");

        worker.send(AgentCommand::ForkClone);
        apply_until(&mut ui_rx, &mut app, |a| {
            a.system_messages
                .iter()
                .any(|(_, m)| sys_msg_text(m).contains("no current session"))
        })
        .await
        .expect("clone 无会话应回 notice 失败");
        shutdown(worker, cmd_tx, handle).await;
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn fork_worker_side_with_real_session() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join("prux-worker-fork");
        let _ = std::fs::create_dir_all(&dir);
        unsafe { std::env::set_var("PRUX_AGENT_DIR", &dir) };
        let entry = crate::core::model_resolver::find_model("deepseek", "deepseek-flash").unwrap();
        let session = session_manager::Session::create("/tmp", Some(dir.clone()), true).unwrap();
        let session_path = session
            .get_session_file()
            .map(|p| p.to_string_lossy().to_string())
            .expect("session 应有文件");
        let agent = Arc::new(tokio::sync::Mutex::new(
            Agent::new(
                entry,
                "/tmp".to_string(),
                ToolSelection::NoBuiltinTools,
                Vec::new(),
                None,
                None,
                None,
                Vec::new(),
                Some(session),
                None,
                false,
                Vec::new(),
            )
            .unwrap(),
        ));

        let (cmd_tx, cmd_rx) = channels();
        let worker = WorkerHandle::new(cmd_tx.clone());
        let (handle, mut ui_rx) = start_worker_with_agent(agent, cmd_rx);
        let mut app = App::new();

        // fork 成功：新会话文件 + SessionSwitched 回执 + 成功提示（/fork 走 ForkFrom）
        worker.send(AgentCommand::ForkFrom {
            path: session_path,
            dir: None,
        });
        apply_until(&mut ui_rx, &mut app, |a| {
            a.system_messages
                .iter()
                .any(|(_, m)| sys_msg_text(m).contains("created new session via fork"))
        })
        .await
        .expect("fork 应创建新会话并提示");
        assert!(
            app.current_session_path.as_deref().is_some(),
            "fork 后应切换会话路径"
        );

        // clone：新会话 main lane 无 leaf → 报错而非崩溃
        worker.send(AgentCommand::ForkClone);
        apply_until(&mut ui_rx, &mut app, |a| {
            a.system_messages
                .iter()
                .any(|(_, m)| sys_msg_text(m).contains("no leaf to clone at"))
        })
        .await
        .expect("clone 无 leaf 应报错");
        shutdown(worker, cmd_tx, handle).await;
    }

    /// 回归：`/new` 的回执必须带上新建会话的文件路径。
    ///
    /// `Session::create(persist=true)` 此刻已经落盘；回执若报 `file: None`，
    /// 子代理扩展会把会话键退化成 `default`（所有无键会话共用 default.json），
    /// 该会话的定时任务之后 `/resume` 回来就读不到了。
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn new_session_reply_carries_the_fresh_session_file() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("prux-worker-new-{}", now_ms()));
        let _ = std::fs::create_dir_all(&dir);
        unsafe { std::env::set_var("PRUX_AGENT_DIR", &dir) };
        let entry = crate::core::model_resolver::find_model("deepseek", "deepseek-flash").unwrap();
        let old = session_manager::Session::create("/tmp", Some(dir.clone()), true).unwrap();
        let old_path = old
            .get_session_file()
            .map(|p| p.to_string_lossy().to_string())
            .expect("旧会话应有文件");
        let agent = Arc::new(tokio::sync::Mutex::new(
            Agent::new(
                entry,
                "/tmp".to_string(),
                ToolSelection::NoBuiltinTools,
                Vec::new(),
                None,
                None,
                None,
                Vec::new(),
                Some(old),
                None,
                false,
                Vec::new(),
            )
            .unwrap(),
        ));

        let (cmd_tx, cmd_rx) = channels();
        let worker = WorkerHandle::new(cmd_tx.clone());
        let (handle, mut ui_rx) = start_worker_with_agent(agent, cmd_rx);
        let mut app = App::new();

        worker.send(AgentCommand::NewSession);
        apply_until(&mut ui_rx, &mut app, |a| {
            a.current_session_path.as_deref().is_some()
        })
        .await
        .expect("/new 后应带上新会话文件路径");

        let new_path = app.current_session_path.clone().unwrap();
        assert_ne!(new_path, old_path, "应是新会话文件");
        assert!(
            std::path::Path::new(&new_path).exists(),
            "回执里的会话文件应已落盘: {new_path}"
        );
        shutdown(worker, cmd_tx, handle).await;
    }

    /// 只读请求头后永不响应的 LLM mock：让 stream_chat 一直 pending，模拟进行中的生成。
    async fn hang_llm_server() -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::AsyncReadExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            loop {
                let n = socket.read(&mut tmp).await.unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            std::future::pending::<()>().await
        });
        (format!("http://127.0.0.1:{}", addr.port()), handle)
    }

    /// 硬中止（Esc）：本轮取消信号必须置位，持有 `parent_abort` 的扩展/工具（如前台
    /// 子代理的 `acquire_foreground_slot` / `run_linked`）才会联动取消；
    /// 且换新信号后 `exec_ctx` 不再带已取消标记，避免污染后续经缓存 ctx 的 spawn。
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn hard_abort_signals_round_parent_abort() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let agent = test_agent_arc();

        let seen: Arc<std::sync::Mutex<Option<Arc<std::sync::atomic::AtomicBool>>>> =
            Arc::new(std::sync::Mutex::new(None));
        let seen2 = seen.clone();
        let (tx, rx) = oneshot::channel();
        let round = run_abortable_round(&agent, rx, move |g| {
            *seen2.lock().unwrap() = Some(g.run_abort.clone());
            Box::pin(std::future::pending::<Result<String>>())
        });

        // 等回合进入（发布 parent_abort）后再中止
        for _ in 0..100 {
            if seen.lock().unwrap().is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        tx.send(()).unwrap();
        round.await;

        let round_flag = seen
            .lock()
            .unwrap()
            .clone()
            .expect("回合应已发布 parent_abort");
        assert!(
            round_flag.load(std::sync::atomic::Ordering::Relaxed),
            "硬中止必须置位本轮 parent_abort"
        );

        // 陈旧 ctx 防护：agent 当前执行上下文已是全新未置位信号
        let ctx_flag = agent.lock().await.exec_ctx().parent_abort;
        assert!(
            !ctx_flag.load(std::sync::atomic::Ordering::Relaxed),
            "硬中止后 exec_ctx 不得复用已置位信号"
        );
        assert!(!Arc::ptr_eq(&ctx_flag, &round_flag), "应为不同 Arc");
    }

    /// prompt 进行中按 Esc（Abort）：prompt future 被 drop，`run_loop` 的
    /// operation_finished 随之丢失——回归测试：中止后会话不得残留未闭合 operation。
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn abort_during_prompt_closes_round_operation() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("prux-abort-{}", now_ms()));
        std::fs::create_dir_all(&tmp).unwrap();
        let cwd = tmp.to_string_lossy().to_string();
        let sess_dir = tmp.join("sessions");
        let session =
            crate::core::session_manager::Session::create(&cwd, Some(sess_dir.clone()), true)
                .unwrap();

        let (base, _hang) = hang_llm_server().await;
        let mut entry =
            crate::core::model_resolver::find_model("deepseek", "deepseek-flash").unwrap();
        entry.base_url = base;
        let agent = Agent::new(
            entry,
            cwd.clone(),
            ToolSelection::NoBuiltinTools,
            Vec::new(),
            None,
            None,
            None,
            Vec::new(),
            Some(session),
            Some("sk-test".to_string()),
            false,
            Vec::new(),
        )
        .unwrap();
        let agent = Arc::new(tokio::sync::Mutex::new(agent));

        let (cmd_tx, cmd_rx) = channels();
        let worker = WorkerHandle::new(cmd_tx.clone());
        let (handle, _ui_rx) = start_worker_with_agent(agent.clone(), cmd_rx);

        worker.prompt("hi".into());
        // 等 operation_started 落盘（run_loop 首个 poll 同步写 START，随后挂在 LLM 流上）。
        // 注意：轮次持有 agent 锁，这里只能轮询文件，不能 lock agent。
        let path = list_sessions(&sess_dir).pop().expect("session file");
        let mut seen_start = false;
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let content = std::fs::read_to_string(&path).unwrap_or_default();
            if content.contains("\"type\":\"operation_started\"") {
                seen_start = true;
                break;
            }
        }
        assert!(seen_start, "run 应已写入 operation_started");

        // Esc：abort 抢先，prompt future 被 drop
        worker.abort();
        let mut seen_finish = false;
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let content = std::fs::read_to_string(&path).unwrap_or_default();
            if content.contains("\"type\":\"operation_finished\"") {
                seen_finish = true;
                break;
            }
        }
        assert!(seen_finish, "abort 后应补写 operation_finished");
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            content.contains("interrupted"),
            "outcome 应为 interrupted: {content}"
        );
        let opened = Session::open(&path.to_string_lossy()).unwrap();
        assert_eq!(opened.open_operations_count(), 0);
        assert!(opened.reduce_lane().is_ok());

        clear_event_loop_tx();
        drop(worker);
        drop(cmd_tx);
        let _ = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("worker 应退出");
    }

    /// prompt 进行中 /quit（或 Ctrl+D）：worker 退出前必须丢弃进行中的回合
    /// 并为未闭合 operation 补写 interrupted，否则下次打开就是僵尸条目。
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn shutdown_during_prompt_closes_open_operation() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("prux-shutdown-{}", now_ms()));
        std::fs::create_dir_all(&tmp).unwrap();
        let cwd = tmp.to_string_lossy().to_string();
        let sess_dir = tmp.join("sessions");
        let session =
            crate::core::session_manager::Session::create(&cwd, Some(sess_dir.clone()), true)
                .unwrap();

        let (base, _hang) = hang_llm_server().await;
        let mut entry =
            crate::core::model_resolver::find_model("deepseek", "deepseek-flash").unwrap();
        entry.base_url = base;
        let agent = Agent::new(
            entry,
            cwd.clone(),
            ToolSelection::NoBuiltinTools,
            Vec::new(),
            None,
            None,
            None,
            Vec::new(),
            Some(session),
            Some("sk-test".to_string()),
            false,
            Vec::new(),
        )
        .unwrap();
        let agent = Arc::new(tokio::sync::Mutex::new(agent));

        let (cmd_tx, cmd_rx) = channels();
        let worker = WorkerHandle::new(cmd_tx.clone());
        let (handle, _ui_rx) = start_worker_with_agent(agent, cmd_rx);

        worker.prompt("hi".into());
        let path = list_sessions(&sess_dir).pop().expect("session file");
        let mut seen_start = false;
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            if std::fs::read_to_string(&path)
                .unwrap_or_default()
                .contains("\"type\":\"operation_started\"")
            {
                seen_start = true;
                break;
            }
        }
        assert!(seen_start, "run 应已写入 operation_started");

        // 退出信号：worker 应立即收尾并退出（不等待挂起的 LLM 流）
        worker.send(AgentCommand::Shutdown);
        let _ = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("Shutdown 后 worker 应退出");

        clear_event_loop_tx();
        drop(worker);
        drop(cmd_tx);

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            content.contains("\"type\":\"operation_finished\"") && content.contains("interrupted"),
            "退出前应补写 interrupted finish: {content}"
        );
        let opened = Session::open(&path.to_string_lossy()).unwrap();
        assert_eq!(opened.open_operations_count(), 0, "退出后不应残留 open op");
        assert!(opened.reduce_lane().is_ok());
    }

    /// 造一条带 usage 的消息（`tokens` 挂在 input 上，成本由 `cost` 控制）
    fn usage_msg(role: &str, provider: &str, model: &str, cost: f64, tokens: u32) -> AgentMessage {
        let mut m = AgentMessage::user_text("x");
        m.role = role.to_string();
        m.provider = Some(provider.to_string());
        m.model = Some(model.to_string());
        m.usage = Some(Usage {
            input: tokens,
            total_tokens: tokens,
            cost: crate::core::provider::Cost {
                input: cost,
                total: cost,
                ..Default::default()
            },
            ..Default::default()
        });
        m
    }

    fn summary_entry(ty: &str, cost: f64, tokens: u32) -> Value {
        let m = usage_msg("assistant", "unused", "unused", cost, tokens);
        json!({ "type": ty, "id": "e1", "usage": m.usage.unwrap() })
    }

    /// `LaneRecord::UsageRecord` 的 JSON 形态（`get_entries()` 的原样输出）：
    /// `attribution = None` 模拟旧会话里没有 provider/model 的记录。
    fn usage_record_entry(attribution: Option<(&str, &str)>, cost: f64, tokens: u32) -> Value {
        let usage = usage_msg("assistant", "unused", "unused", cost, tokens)
            .usage
            .unwrap();
        let mut v = json!({ "type": "usage_record", "cause": "cache_warm", "usage": usage });
        if let Some((provider, model)) = attribution {
            v["provider"] = json!(provider);
            v["model"] = json!(model);
        }
        v
    }

    #[test]
    fn cost_breakdown_includes_cache_warm_usage_and_reconciles_with_totals() {
        // 保温用量不经会话回合（不落 assistant 消息），只能从 usage_record 条目拿；
        // 带 provider/model 归属的按模型归集，旧记录（无归属）进 Tools 桶；
        // 并且「消息 + 条目」的合计必须等于分列之和。
        let messages = vec![usage_msg(
            "assistant",
            "anthropic",
            "claude-sonnet-5-5",
            0.30,
            2_000,
        )];
        let entries = vec![
            summary_entry("compaction", 0.02, 300),
            usage_record_entry(Some(("anthropic", "claude-sonnet-5-5")), 0.05, 1_000),
            usage_record_entry(None, 0.01, 100),
        ];

        let mut input = 0u64;
        let mut output = 0u64;
        let mut cache_read = 0u64;
        let mut cache_write = 0u64;
        let mut cost = 0.0f64;
        accumulate_session_usage(
            &entries,
            &mut input,
            &mut output,
            &mut cache_read,
            &mut cache_write,
            &mut cost,
        );
        assert!((cost - 0.08).abs() < 1e-9, "条目侧合计: {cost}");
        assert_eq!(input, 1_400, "三条条目各 300/1000/100 tokens");

        let breakdown = usage_cost_breakdown(&messages, &entries);
        let keys: Vec<&str> = breakdown.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys, vec!["anthropic/claude-sonnet-5-5", "Tools/summaries"]);
        // assistant 0.30 + 保温 0.05 归到同一模型
        assert!((breakdown[0].cost - 0.35).abs() < 1e-9, "{breakdown:?}");
        assert_eq!(breakdown[0].tokens, 3_000);
        // 摘要 0.02 + 无归属保温 0.01
        assert!((breakdown[1].cost - 0.03).abs() < 1e-9, "{breakdown:?}");

        let sum: f64 = breakdown.iter().map(|e| e.cost).sum();
        assert!(
            (sum - (0.30 + cost)).abs() < 1e-9,
            "分列之和应等于会话合计: {sum} vs {}",
            0.30 + cost
        );
    }

    #[test]
    fn cost_breakdown_groups_by_provider_model_and_tools_bucket() {
        // 对齐 pi `getUsageCostBreakdown`：assistant 按 provider/模型分组，
        // toolResult 与摘要条目进 `Tools/summaries`，按成本降序。
        let mut gateway = usage_msg("assistant", "openrouter", "auto", 0.05, 100);
        gateway.response_model = Some("anthropic/claude-opus-5-5".into());
        let messages = vec![
            usage_msg("assistant", "anthropic", "claude-sonnet-5-5", 0.30, 2_000),
            usage_msg("assistant", "anthropic", "claude-sonnet-5-5", 0.10, 500),
            gateway,
            usage_msg("toolResult", "anthropic", "claude-sonnet-5-5", 0.04, 800),
            usage_msg("user", "anthropic", "claude-sonnet-5-5", 0.0, 0),
        ];
        let entries = vec![
            summary_entry("compaction", 0.02, 300),
            summary_entry("branch_summary", 0.01, 100),
            json!({ "type": "message", "id": "e9" }),
        ];

        let breakdown = usage_cost_breakdown(&messages, &entries);
        let keys: Vec<&str> = breakdown.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(
            keys,
            vec![
                "anthropic/claude-sonnet-5-5",
                "Tools/summaries",
                "openrouter/anthropic/claude-opus-5-5",
            ]
        );
        assert!((breakdown[0].cost - 0.40).abs() < 1e-9, "{breakdown:?}");
        assert_eq!(breakdown[0].tokens, 2_500);
        assert!((breakdown[1].cost - 0.07).abs() < 1e-9, "{breakdown:?}");
        assert_eq!(breakdown[1].tokens, 1_200);
        assert!((breakdown[2].cost - 0.05).abs() < 1e-9, "{breakdown:?}");
    }

    #[test]
    fn cost_breakdown_drops_zero_usage_entries() {
        // 零成本零 token 的条目不上报（否则分列里会多出一堆 $0.000 行）
        let messages = vec![usage_msg(
            "assistant",
            "anthropic",
            "claude-sonnet-5-5",
            0.0,
            0,
        )];
        assert!(usage_cost_breakdown(&messages, &[]).is_empty());
    }
}
