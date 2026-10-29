//! agent worker：独占持有 `Agent` 的 tokio 任务（GUI 版）。
//!
//! 与 TUI 的 `interactive::worker` 同构但更薄：回合内的状态更新不投给 UI 闭包，
//! 而是把 agent 内核的 JSON 事件原样送进 GUI 的通道，由视图层自己解释；
//! 回合外的命令（切模型、切会话、压缩…）在这里就地执行，执行完推一份状态快照。
//! 回合被 `Abort` 打断的方式也与 TUI 一致——直接 drop 底层 prompt future。

use super::{GuiEvent, Snapshot, UsageTotals};

use crate::{
    core::{
        self,
        agent_session::Agent,
        bug_report,
        compaction::{CompactionReason, estimate_context_tokens},
        crash_log, export_html,
        extensions::dispatch_exec_ctx,
        provider::AgentMessage,
        session_manager::{self, OpenSessionResult, Session},
        settings_manager, skills,
    },
    extensions::skills::bundled_skill_names,
    modes::interactive::{
        agent_actor::{AgentCommand, CommandRx},
        worker::{session_tree_text, share_via_gist},
    },
    utils::paths::home_dir,
};
use std::{
    collections::{HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::{Mutex, mpsc::UnboundedSender};

/// 回合 future：持锁跑完一轮，drop 即取消。
type RoundFuture = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;

/// 一轮要跑的工作（都需要独占 agent，都可被 Abort 打断）。
enum Task {
    /// 纯文本 prompt
    Text(String),
    /// 单条消息（含附件）；AgentMessage 体积大，装箱避免整个枚举变胖
    Message(Box<AgentMessage>),
    /// 多条消息（初始队列 / steer 批）
    Batch(Vec<AgentMessage>),
    /// 手动压缩（可带自定义指令）
    Compact(Option<String>),
    /// 会话树导航
    Navigate { target_id: String, summarize: bool },
}

/// worker 主循环：队列化命令，回合中只接受中止。
pub async fn run(
    agent: Arc<Mutex<Agent>>,
    mut cmd_rx: CommandRx,
    events: UnboundedSender<GuiEvent>,
) {
    {
        let mut agent = agent.lock().await;
        let sink = events.clone();
        agent.attach_json_sink(Box::new(move |ev| {
            _ = sink.send(GuiEvent::Agent(ev));
        }));
        publish(&agent, &events);
    }

    let mut on_round: Option<RoundFuture> = None;
    let mut queue: VecDeque<AgentCommand> = VecDeque::new();
    // 本回合开始前已打开的 operation：中断后据此只收尾本轮新开的那些
    let mut round_ops: Option<HashSet<String>> = None;

    loop {
        if let Some(fut) = on_round.as_mut() {
            tokio::select! {
                cmd = cmd_rx.recv() => match cmd {
                    // 回合进行中：中止类命令丢弃 turn future；其余排队等回合结束
                    Some(AgentCommand::Abort) | Some(AgentCommand::Shutdown) => {
                        on_round = None;
                        hard_abort(&agent, round_ops.take()).await;
                    }
                    None => break,
                    Some(cmd) => queue.push_back(cmd),
                },
                _ = fut => on_round = None,
            }
            continue;
        }

        let cmd = match queue.pop_front() {
            Some(cmd) => cmd,
            None => match cmd_rx.recv().await {
                Some(cmd) => cmd,
                None => break,
            },
        };

        // 回合类命令先归约成 Task，非回合命令就地执行完继续下一轮
        let task = match cmd {
            AgentCommand::Prompt(text) => {
                // `/skill:名称` 由技能正文展开成真正的提示词（与 TUI 同一套规则）
                let text = {
                    let agent = agent.lock().await;
                    skills::expand_skill_command(&text, &agent.skills)
                };
                Task::Text(text)
            }
            AgentCommand::PromptMessage(msg) => Task::Message(Box::new(msg)),
            AgentCommand::PromptBatch(msgs) => Task::Batch(msgs),
            AgentCommand::Compact { instructions } => Task::Compact(instructions),
            AgentCommand::Navigate {
                target_id,
                summarize,
            } => Task::Navigate {
                target_id,
                summarize,
            },
            AgentCommand::Shutdown => break,
            AgentCommand::Abort => continue,
            cmd => {
                let mut agent = agent.lock().await;
                apply(&mut agent, cmd, &events).await;
                publish(&agent, &events);
                continue;
            }
        };
        round_ops = Some(open_operations(&agent).await);
        on_round = Some(start(&agent, task, &events));
    }
}

/// 硬中止的收尾：回合 future 已被 drop，`run_loop` 的 `operation_finished`
/// 与 `agent_settled` 都不会再来，这里补位本轮信号与未闭合的 operation。
async fn hard_abort(agent: &Arc<Mutex<Agent>>, before: Option<HashSet<String>>) {
    let mut agent = agent.lock().await;
    agent.abort_and_detach();

    dispatch_exec_ctx(&agent.exec_ctx());
    if let (Some(session), Some(before)) = (agent.session.as_mut(), before) {
        _ = session.interrupt_operations_opened_since(&before);
    }
}

/// 回合开始时记下已打开的 operation id，供中止后识别本轮新开的那些。
async fn open_operations(agent: &Arc<Mutex<Agent>>) -> HashSet<String> {
    let agent = agent.lock().await;
    agent
        .session
        .as_ref()
        .map(|session| session.open_operation_ids().into_iter().collect())
        .unwrap_or_default()
}

/// 非回合命令：就地改 agent 状态。
async fn apply(agent: &mut Agent, cmd: AgentCommand, events: &UnboundedSender<GuiEvent>) {
    match cmd {
        AgentCommand::SwitchModel {
            provider,
            model_id,
            persist,
        } => {
            if let Err(e) = agent.switch_model(&provider, &model_id, persist) {
                _ = events.send(GuiEvent::Error(e.to_string()));
            }
        }
        AgentCommand::SetThinking { level, persist } => {
            agent.set_thinking_level_silent(&level, persist);
        }
        AgentCommand::SetSessionName { name } => {
            agent.set_session_name(&name);
        }
        AgentCommand::SwitchCwd { path } => switch_cwd(agent, &path, events),
        AgentCommand::Reload { .. } => reload_assets(agent, events),
        AgentCommand::Tree => {
            let message = match agent.session.as_ref() {
                Some(session) => session_tree_text(session),
                None => "No current session".to_string(),
            };
            _ = events.send(GuiEvent::Notice(message));
        }
        AgentCommand::DebugInfo => {
            let mut lines = vec![
                format!("provider: {}", agent.model.provider),
                format!("model: {}", agent.model.model_id),
                format!("messages: {}", agent.messages.len()),
                format!("autoCompaction: {}", agent.auto_compaction),
                format!("autoRetry: {}", agent.auto_retry),
                format!("cwd: {}", agent.cwd),
            ];

            if let Some(session) = agent.session.as_ref() {
                lines.push(format!("session: {}", session.session_id));
                lines.push(format!("leaf: {:?}", session.get_leaf_id()));
            }
            _ = events.send(GuiEvent::Notice(lines.join(" · ")));
        }
        AgentCommand::BugReport {
            hint,
            include_session,
            include_summary,
        } => write_bug_report(agent, hint, include_session, include_summary, events),
        AgentCommand::Label { entry_id, label } => match agent.session.as_mut() {
            Some(session) => match session.append_label_change(&entry_id, label.as_deref()) {
                Ok(_) => {
                    let message = match label {
                        Some(text) => format!("Labeled {entry_id}: {text}"),
                        None => format!("Cleared label on {entry_id}"),
                    };
                    _ = events.send(GuiEvent::Notice(message));
                }
                Err(error) => {
                    _ = events.send(GuiEvent::Notice(format!("Label failed: {error}")));
                }
            },
            None => {
                _ = events.send(GuiEvent::Notice("No current session".into()));
            }
        },
        AgentCommand::ForkClone => clone_session(agent, events),
        AgentCommand::ForkFrom { path, .. } => fork_session(agent, &path, events),
        AgentCommand::Share { theme } => {
            let message = match agent.session.as_ref() {
                Some(session) => {
                    match settings_manager::read_settings_session_share_destination() {
                        settings_manager::SessionShareDestination::Radius => {
                            "Share failed: Radius upload is not supported; set sessionShareDestination to \"gist\"".to_string()
                        }
                        _ => share_via_gist(
                            session,
                            theme.as_deref(),
                        ),
                    }
                }
                None => "Share failed: no current session".to_string(),
            };
            _ = events.send(GuiEvent::Notice(message));
        }
        AgentCommand::Export { out, theme } => {
            let message = match agent.session.as_ref() {
                Some(session) => {
                    match export_html::export_session(session, out.as_deref(), theme.as_deref()) {
                        Ok(path) => format!("Exported: {path}"),
                        Err(error) => format!("Export failed: {error}"),
                    }
                }
                None => "No current session to export".to_string(),
            };
            _ = events.send(GuiEvent::Notice(message));
        }
        AgentCommand::NewSession => new_session(agent, events),
        AgentCommand::ResumeSession { path } => resume_session(agent, &path, events),
        AgentCommand::Rewind => {
            if let Err(e) = agent.rewind_last_user_input() {
                _ = events.send(GuiEvent::Error(e.to_string()));
            }
        }
        // 其余命令（登录、导出、分享、bug report、fork、reload…）尚未在 GUI 侧接线
        _ => {}
    }
}

/// `/bug`：收集脱敏元数据 + 诊断打成 zip（桌面端不生成模型摘要）。
fn write_bug_report(
    agent: &mut Agent,
    hint: String,
    include_session: bool,
    include_summary: bool,
    events: &UnboundedSender<GuiEvent>,
) {
    let hint = (!hint.trim().is_empty()).then_some(hint);
    let crashes = crash_log::read_crash_log();
    let has_crashes = !crashes.is_empty();
    let ctx = bug_report::BugReportContext {
        hint,
        session_id: agent
            .session
            .as_ref()
            .map(|session| session.session_id.clone())
            .unwrap_or_default(),
        cwd: agent.cwd.clone(),
        session_file: agent
            .session
            .as_ref()
            .and_then(|session| session.get_session_file())
            .map(|path| path.to_string_lossy().to_string()),
        include_session,
        // 桌面端不调模型生成摘要，避免在打包路径里发起一次请求
        include_summary: false,
        message_count: agent.messages.len(),
        model: &agent.model,
        thinking_level: agent.thinking_level.clone(),
    };

    let bundle = bug_report::build_bundle(bug_report::BundleInput {
        ctx,
        session: agent.session.as_ref(),
        crashes: &crashes,
        summary: None,
    });
    let id = bundle
        .metadata
        .get("id")
        .and_then(|value| value.as_str())
        .unwrap_or("unknown")
        .to_string();

    let path = PathBuf::from(&agent.cwd).join(bug_report::archive_file_name(&id));
    if let Err(error) = bug_report::write_bug_report_archive(&bundle, &path) {
        _ = events.send(GuiEvent::Notice(format!(
            "Cannot write bug report: {error}"
        )));
        return;
    }

    if let Some(session) = agent.session.as_mut() {
        session.append_custom_entry(
            bug_report::BUG_REPORT_CUSTOM_ENTRY_TYPE,
            Some(serde_json::json!({
                "id": id,
                "sessionIncluded": include_session,
                "summaryIncluded": false,
                "delivery": "zip",
                "path": path.to_string_lossy(),
            })),
        );
    }

    if has_crashes {
        crash_log::clear_crash_log();
    }

    _ = events.send(GuiEvent::Notice(if include_summary {
        format!(
            "Bug report exported (no model summary on desktop): {}",
            path.display()
        )
    } else {
        format!("Bug report exported: {}", path.display())
    }));
}

/// `/reload`：重读扩展启用状态、上下文文件、技能，再按新内容重建工具与系统提示。
///
/// 只覆盖 GUI 能自己决定的部分（默认技能发现、当前信任状态）；命令行给的
/// `--skill`/`--no-context-files` 这类启动参数没有在桌面端留副本，重载后会回到默认。
fn reload_assets(agent: &mut Agent, events: &UnboundedSender<GuiEvent>) {
    let agent_dir = core::settings_manager::agent_dir();
    let home_dir = home_dir();
    let cwd = agent.cwd.clone();
    let trusted = core::project_trust::is_project_trusted(Path::new(&cwd), &agent_dir);

    core::extensions::reload_from_settings();

    let settings = settings_manager::read_settings();
    agent.skills = core::skills::load_skills(
        &cwd,
        &agent_dir,
        &home_dir,
        &[],
        &bundled_skill_names(),
        true,
        trusted,
        &settings.skills,
    );

    let files = core::context::load_project_context_files(&cwd, &agent_dir);
    agent.rebuild_ctx.context_files = if trusted {
        files
    } else {
        files
            .into_iter()
            .filter(|(path, _)| Path::new(path).starts_with(&agent_dir))
            .collect()
    };
    agent.rebuild_tools();
    _ = events.send(GuiEvent::Notice(format!(
        "Reloaded: {} skills, {} context files",
        agent.skills.len(),
        agent.rebuild_ctx.context_files.len()
    )));
}

/// 把 fork/clone 出来的新会话接到 agent 上，并把历史推给 GUI。
fn adopt_session(
    agent: &mut Agent,
    session: Session,
    events: &UnboundedSender<GuiEvent>,
    what: &str,
) {
    agent.messages = session.build_context_messages();
    agent.session = Some(session);
    agent.restore_session_tools();
    core::extensions::dispatch_exec_ctx(&agent.exec_ctx());
    _ = events.send(GuiEvent::Notice(what.to_string()));
    _ = events.send(GuiEvent::History(agent.messages.clone()));
}

/// `/clone`：按当前分支叶子复制一份新会话。
fn clone_session(agent: &mut Agent, events: &UnboundedSender<GuiEvent>) {
    let cwd = agent.cwd.clone();
    let source = agent
        .session
        .as_ref()
        .and_then(|session| session.get_session_file())
        .map(|path| path.to_string_lossy().to_string());
    let leaf = agent
        .session
        .as_ref()
        .and_then(|session| session.clone_target_id());
    match (source, leaf) {
        (Some(path), Some(leaf)) => match Session::fork_at(&path, &leaf, &cwd, None) {
            Ok(session) => adopt_session(agent, session, events, "Cloned current branch"),
            Err(error) => {
                _ = events.send(GuiEvent::Notice(format!("Clone failed: {error}")));
            }
        },
        (Some(_), None) => {
            _ = events.send(GuiEvent::Notice("Clone failed: nothing to clone".into()));
        }
        (None, _) => {
            _ = events.send(GuiEvent::Notice("Clone failed: no current session".into()));
        }
    }
}

/// `/fork`：整树复制当前会话。
fn fork_session(agent: &mut Agent, path: &str, events: &UnboundedSender<GuiEvent>) {
    let cwd = agent.cwd.clone();
    match Session::fork_from(path, &cwd, None) {
        Ok(session) => adopt_session(agent, session, events, "Forked current session"),
        Err(error) => {
            _ = events.send(GuiEvent::Notice(format!("Fork failed: {error}")));
        }
    }
}

/// 切工作目录：改 agent 的 cwd、按新目录重建工具与系统提示，并在新目录下开会话。
fn switch_cwd(agent: &mut Agent, path: &str, events: &UnboundedSender<GuiEvent>) {
    let dir = Path::new(path);
    if !dir.is_dir() {
        _ = events.send(GuiEvent::Error(format!(
            "Working directory does not exist: {path}"
        )));
        return;
    }

    agent.cwd = path.to_string();
    agent.rebuild_tools();
    new_session(agent, events);
}

/// 开新会话：清空消息、建一个持久会话文件（与 TUI 的 `/new` 一致）。
fn new_session(agent: &mut Agent, events: &UnboundedSender<GuiEvent>) {
    let cwd = agent.cwd.clone();
    let dir = session_manager::default_session_dir(&cwd, &settings_manager::agent_dir());

    _ = std::fs::create_dir_all(&dir);

    match Session::create_deferred(&cwd, Some(dir)) {
        Ok(session) => {
            agent.messages.clear();
            agent.session = Some(session);
            core::extensions::dispatch_exec_ctx(&agent.exec_ctx());
            _ = events.send(GuiEvent::History(Vec::new()));
        }
        Err(e) => {
            _ = events.send(GuiEvent::Error(e.to_string()));
        }
    }
}

/// 切到已有会话：读回历史消息（中间行损坏的会话直接报错，修复面板留给后续）。
fn resume_session(agent: &mut Agent, path: &str, events: &UnboundedSender<GuiEvent>) {
    match Session::open_checked(path) {
        OpenSessionResult::Ok(session) => {
            agent.messages = session.build_context_messages();
            agent.session = Some(*session);
            agent.restore_session_tools();
            core::extensions::dispatch_exec_ctx(&agent.exec_ctx());
            _ = events.send(GuiEvent::History(agent.messages.clone()));
        }
        OpenSessionResult::Invalid(e) => {
            _ = events.send(GuiEvent::Error(format!("Cannot open session: {e}")));
        }
        OpenSessionResult::Repairable { plan, .. } => {
            _ = events.send(GuiEvent::Error(format!(
                "Session has a corrupt entry; needs repair: {}",
                plan.error
            )));
        }
    }
}

/// 跑一轮：出错补一条 [`GuiEvent::Error`]，结束后推状态快照。
fn start(agent: &Arc<Mutex<Agent>>, task: Task, events: &UnboundedSender<GuiEvent>) -> RoundFuture {
    let agent = agent.clone();
    let events = events.clone();

    Box::pin(async move {
        let mut agent = agent.lock().await;
        let result = match task {
            Task::Text(text) => agent.prompt(&text).await.map(|_| ()),
            Task::Message(msg) => agent.prompt_message(*msg).await.map(|_| ()),
            Task::Batch(msgs) => agent.prompt_messages(msgs).await.map(|_| ()),
            Task::Compact(instructions) => agent
                .maybe_compact_with(CompactionReason::Manual, instructions.as_deref())
                .await
                .map(|_| ()),
            Task::Navigate {
                target_id,
                summarize,
            } => agent.navigate_tree(&target_id, summarize).await.map(|_| ()),
        };

        if let Err(e) = result {
            _ = events.send(GuiEvent::Error(e.to_string()));
        }
        publish(&agent, &events);
    })
}

/// 推一份状态快照给 GUI。
fn publish(agent: &Agent, events: &UnboundedSender<GuiEvent>) {
    _ = events.send(GuiEvent::Snapshot(Box::new(snapshot(agent))));
}

/// 从 agent 采集 GUI 需要的状态。
fn snapshot(agent: &Agent) -> Snapshot {
    let mut usage = UsageTotals::default();
    for message in &agent.messages {
        let Some(u) = &message.usage else { continue };
        usage.input += u64::from(u.input);
        usage.output += u64::from(u.output);
        usage.cache_read += u64::from(u.cache_read);
        usage.cache_write += u64::from(u.cache_write);
        usage.cost += u.cost.total;
    }

    // 上下文占用走历史估算（末条 assistant usage + 其后新增消息），
    // 刚载入的会话还没有 `last_usage`，只有走它状态栏才显示得出占用率
    usage.context_tokens = u64::from(estimate_context_tokens(&agent.messages).tokens);
    usage.context_window = u64::from(agent.model.context_window);

    Snapshot {
        provider: agent.model.provider.clone(),
        model: agent.model.model_id.clone(),
        thinking: agent.thinking_level.clone(),
        thinking_levels: agent
            .get_available_thinking_levels()
            .into_iter()
            .map(str::to_string)
            .collect(),
        model_cycle: agent.model_cycle.clone(),
        session_path: agent
            .session
            .as_ref()
            .and_then(|s| s.get_session_file())
            .map(|p| p.to_string_lossy().to_string()),
        messages: agent.messages.len(),
        leaf_entry_id: agent.session.as_ref().and_then(|s| s.clone_target_id()),
        tools: agent.get_active_tools(),
        cwd: agent.cwd.clone(),
        skills: agent
            .skills
            .iter()
            .map(|s| (s.name.clone(), s.description.clone(), s.source.clone()))
            .collect(),
        usage,
    }
}
