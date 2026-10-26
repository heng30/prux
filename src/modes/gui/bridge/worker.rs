//! agent worker：独占持有 `Agent` 的 tokio 任务（GUI 版）。
//!
//! 与 TUI 的 `interactive::worker` 同构但更薄：回合内的状态更新不投给 UI 闭包，
//! 而是把 agent 内核的 JSON 事件原样送进 GUI 的通道，由视图层自己解释；
//! 回合外的命令（切模型、切会话、压缩…）在这里就地执行，执行完推一份状态快照。
//! 回合被 `Abort` 打断的方式也与 TUI 一致——直接 drop 底层 prompt future。

use super::{GuiEvent, Snapshot, UsageTotals};
use crate::core::agent_session::Agent;
use crate::core::compaction::CompactionReason;
use crate::core::provider::AgentMessage;
use crate::core::session_manager::{self, OpenSessionResult, Session};
use crate::core::settings_manager;
use crate::modes::interactive::agent_actor::{AgentCommand, CommandRx};
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::mpsc::UnboundedSender;

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

/// 回合 future：持锁跑完一轮，drop 即取消。
type RoundFuture = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;

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
            let _ = sink.send(GuiEvent::Agent(ev));
        }));
        publish(&agent, &events);
    }

    let mut on_round: Option<RoundFuture> = None;
    let mut queue: VecDeque<AgentCommand> = VecDeque::new();

    loop {
        if let Some(fut) = on_round.as_mut() {
            tokio::select! {
                cmd = cmd_rx.recv() => match cmd {
                    // 回合进行中：中止类命令丢弃 turn future；其余排队等回合结束
                    Some(AgentCommand::Abort) | Some(AgentCommand::Shutdown) => on_round = None,
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

        match cmd {
            AgentCommand::Prompt(text) => on_round = Some(start(&agent, Task::Text(text), &events)),
            AgentCommand::PromptMessage(msg) => {
                on_round = Some(start(&agent, Task::Message(Box::new(msg)), &events))
            }
            AgentCommand::PromptBatch(msgs) => {
                on_round = Some(start(&agent, Task::Batch(msgs), &events))
            }
            AgentCommand::Compact { instructions } => {
                on_round = Some(start(&agent, Task::Compact(instructions), &events))
            }
            AgentCommand::Navigate {
                target_id,
                summarize,
            } => {
                on_round = Some(start(
                    &agent,
                    Task::Navigate {
                        target_id,
                        summarize,
                    },
                    &events,
                ))
            }
            AgentCommand::Shutdown => break,
            AgentCommand::Abort => {}
            cmd => {
                let mut agent = agent.lock().await;
                apply(&mut agent, cmd, &events).await;
                publish(&agent, &events);
            }
        }
    }
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
                let _ = events.send(GuiEvent::Error(e.to_string()));
            }
        }
        AgentCommand::SetThinking { level, persist } => {
            agent.set_thinking_level_silent(&level, persist);
        }
        AgentCommand::SetSessionName { name } => {
            agent.set_session_name(&name);
        }
        AgentCommand::NewSession => new_session(agent, events),
        AgentCommand::ResumeSession { path } => resume_session(agent, &path, events),
        AgentCommand::Rewind => {
            if let Err(e) = agent.rewind_last_user_input() {
                let _ = events.send(GuiEvent::Error(e.to_string()));
            }
        }
        // 其余命令（登录、导出、分享、bug report、fork、reload…）尚未在 GUI 侧接线
        _ => {}
    }
}

/// 开新会话：清空消息、建一个持久会话文件（与 TUI 的 `/new` 一致）。
fn new_session(agent: &mut Agent, events: &UnboundedSender<GuiEvent>) {
    let cwd = agent.cwd.clone();
    let dir = session_manager::default_session_dir(&cwd, &settings_manager::agent_dir());
    _ = std::fs::create_dir_all(&dir);
    match Session::create(&cwd, Some(dir), true) {
        Ok(session) => {
            agent.messages.clear();
            agent.session = Some(session);
            crate::core::extensions::dispatch_exec_ctx(&agent.exec_ctx());
            let _ = events.send(GuiEvent::History(Vec::new()));
        }
        Err(e) => {
            let _ = events.send(GuiEvent::Error(e.to_string()));
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
            crate::core::extensions::dispatch_exec_ctx(&agent.exec_ctx());
            let _ = events.send(GuiEvent::History(agent.messages.clone()));
        }
        OpenSessionResult::Invalid(e) => {
            let _ = events.send(GuiEvent::Error(format!("无法打开会话：{e}")));
        }
        OpenSessionResult::Repairable { plan, .. } => {
            let _ = events.send(GuiEvent::Error(format!(
                "会话中间行损坏，需要修复：{}",
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
            let _ = events.send(GuiEvent::Error(e.to_string()));
        }
        publish(&agent, &events);
    })
}

/// 推一份状态快照给 GUI。
fn publish(agent: &Agent, events: &UnboundedSender<GuiEvent>) {
    let _ = events.send(GuiEvent::Snapshot(Box::new(snapshot(agent))));
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
    if let Some(last) = &agent.last_usage {
        usage.context_tokens = u64::from(last.total_tokens);
    }

    Snapshot {
        provider: agent.model.provider.clone(),
        model: agent.model.model_id.clone(),
        thinking: agent.thinking_level.clone(),
        thinking_levels: agent
            .get_available_thinking_levels()
            .into_iter()
            .map(str::to_string)
            .collect(),
        session_name: agent.session.as_ref().and_then(|s| s.name.clone()),
        session_path: agent
            .session
            .as_ref()
            .and_then(|s| s.get_session_file())
            .map(|p| p.to_string_lossy().to_string()),
        messages: agent.messages.len(),
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
