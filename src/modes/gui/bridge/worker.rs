//! agent worker：独占持有 `Agent` 的 tokio 任务（GUI 版）。
//!
//! 与 TUI 的 `tui::worker` 同构但更薄：回合内的状态更新不投给 UI 闭包，
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
        context_breakdown, crash_log, export_html,
        extensions::dispatch_exec_ctx,
        prompt_templates::{self, PromptTemplate},
        provider::AgentMessage,
        session_manager::{self, OpenSessionResult, Session},
        settings_manager,
    },
    extensions::skills::bundled_skill_names,
    modes::{
        slash::expand_user_text,
        tui::{
            agent_actor::{AgentCommand, CommandRx},
            app::ReloadCtx,
            worker::{
                debug_info_text, debug_tools_text, drain_cache_warm, session_tree_text,
                share_via_gist,
            },
        },
    },
    utils::paths::home_dir,
};
use std::{
    collections::{HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::sync::{Mutex, mpsc::UnboundedSender};

/// 空闲时多久去取一次提示缓存保温回执：保温在后台 task 里跑，
/// 完成时不会唤醒这个循环（idle 模式可能长时间没有任何命令）。
const CACHE_WARM_POLL: Duration = Duration::from_millis(1000);

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
///
/// `reload_ctx` 是启动参数里决定 `/reload` 能重扫哪些资源的那份快照
/// （`--skill` / `--no-context-files` / CLI 系统提示等），与 TUI 共用同一套语义。
pub async fn run(
    agent: Arc<Mutex<Agent>>,
    mut cmd_rx: CommandRx,
    events: UnboundedSender<GuiEvent>,
    reload_ctx: ReloadCtx,
    mut prompt_templates: Vec<PromptTemplate>,
) {
    {
        let mut agent = agent.lock().await;
        let sink = events.clone();
        agent.attach_json_sink(Box::new(move |ev| {
            _ = sink.send(GuiEvent::Agent(ev));
        }));
        publish(&agent, &events);
        // 启动时就把已载入的会话历史交给视图：否则续接会话打开后对话区是空的，
        // 依赖历史的扩展命令（`/btw`、`/recap` 的摘录）也拿不到内容
        _ = events.send(GuiEvent::History(agent.messages.clone()));
        _ = events.send(GuiEvent::PromptAssets {
            skills: agent.skills.clone(),
            templates: prompt_templates.clone(),
        });
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
                _ = fut => {
                    on_round = None;
                    // 回合结束后保温任务可能已经跑完：先把回执落成会话记录并通知视图
                    drain_cache_warm(&agent).await;
                }
            }
            continue;
        }

        let cmd = match queue.pop_front() {
            Some(cmd) => cmd,
            None => match tokio::time::timeout(CACHE_WARM_POLL, cmd_rx.recv()).await {
                Ok(Some(cmd)) => cmd,
                Ok(None) => break,
                // 空闲轮询：保温刚在后台完成，取回执后继续等命令
                Err(_) => {
                    drain_cache_warm(&agent).await;
                    continue;
                }
            },
        };

        // 回合类命令先归约成 Task，非回合命令就地执行完继续下一轮
        let task = match cmd {
            AgentCommand::Prompt(text) => {
                let expanded =
                    expand_user_text(&text, &agent.lock().await.skills, &prompt_templates);
                Task::Text(expanded)
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
                apply(&mut agent, cmd, &events, &reload_ctx, &mut prompt_templates).await;
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
async fn apply(
    agent: &mut Agent,
    cmd: AgentCommand,
    events: &UnboundedSender<GuiEvent>,
    reload_ctx: &ReloadCtx,
    prompt_templates: &mut Vec<PromptTemplate>,
) {
    match cmd {
        AgentCommand::SwitchModel {
            provider,
            model_id,
            persist,
        } => switch_model(agent, &provider, &model_id, persist, events),
        AgentCommand::BugReport {
            hint,
            include_session,
            include_summary,
        } => write_bug_report(agent, hint, include_session, include_summary, events),
        AgentCommand::AppendCustomEntry { custom_type, data } => {
            append_custom_entry(agent, &custom_type, data)
        }
        AgentCommand::SetThinking { level, persist } => set_thinking(agent, &level, persist),
        AgentCommand::SetSessionName { name } => set_session_name(agent, &name, events),
        AgentCommand::SwitchCwd { path } => switch_cwd(agent, &path, events),
        AgentCommand::SetCwd { path } => set_cwd(agent, &path, events),
        AgentCommand::Reload { .. } => reload_assets(agent, events, reload_ctx, prompt_templates),
        AgentCommand::Tree => show_tree(agent, events),
        AgentCommand::SystemPrompt => show_system_prompt(agent, events),
        AgentCommand::DebugInfo => show_debug_info(agent, events),
        AgentCommand::DebugTools => show_debug_tools(agent, events),
        AgentCommand::ContextBreakdown => show_context_breakdown(agent, events),
        AgentCommand::Label { entry_id, label } => set_label(agent, &entry_id, label, events),
        AgentCommand::ForkClone => clone_session(agent, events),
        AgentCommand::ForkFrom { path, .. } => fork_session(agent, &path, events),
        AgentCommand::Share { theme } => share_session(agent, theme, events),
        AgentCommand::Export { out, theme } => export_session(agent, out, theme, events),
        AgentCommand::ResumeZombie { path, action } => resume_zombie(agent, &path, &action, events),
        AgentCommand::NewSession => new_session(agent, events),
        AgentCommand::ResumeSession { path } => resume_session(agent, &path, events),
        AgentCommand::ApplyKey { provider, key } => apply_key(agent, &provider, key),
        AgentCommand::RebuildTools => rebuild_tools(agent),
        AgentCommand::Rewind => rewind(agent, events),
        _ => {} // 其余命令尚未在 GUI 侧接线
    }
}

/// `/model <provider>/<id>`：切换模型，失败时回执错误。
fn switch_model(
    agent: &mut Agent,
    provider: &str,
    model_id: &str,
    persist: bool,
    events: &UnboundedSender<GuiEvent>,
) {
    if let Err(e) = agent.switch_model(provider, model_id, persist) {
        _ = events.send(GuiEvent::Error(e.to_string()));
    }
}

/// `/thinking <level>`：设置思考级别（不刷提示消息，按键切换与面板走同一入口）。
fn set_thinking(agent: &mut Agent, level: &str, persist: bool) {
    agent.set_thinking_level_silent(level, persist);
}

/// `/name <name>`：设置会话显示名。
/// `/name` 与扩展起名：写会话名后回推快照。
///
/// GUI 的侧栏靠快照里 `session_name` 的前后差异重读会话列表（见 `on_snapshot`），
/// 不推快照就只有会话文件改了、侧栏还是旧标题。
fn set_session_name(agent: &mut Agent, name: &str, events: &UnboundedSender<GuiEvent>) {
    agent.set_session_name(name);
    _ = events.send(GuiEvent::Snapshot(Box::new(snapshot(agent))));
}

/// `/tree`：把会话树排成文本回执（无会话时给一行提示）。
fn show_tree(agent: &Agent, events: &UnboundedSender<GuiEvent>) {
    let message = match agent.session.as_ref() {
        Some(session) => session_tree_text(session),
        None => "No current session".to_string(),
    };
    _ = events.send(GuiEvent::Notice(message));
}

/// `/debug system-prompt`：回执当前系统提示词原文。
fn show_system_prompt(agent: &Agent, events: &UnboundedSender<GuiEvent>) {
    let prompt = agent.system_prompt.trim();
    let message = if prompt.is_empty() {
        "system prompt is empty".to_string()
    } else {
        prompt.to_string()
    };
    _ = events.send(GuiEvent::Notice(message));
}

/// `/debug info`：回执 agent 运行状态汇总。
fn show_debug_info(agent: &Agent, events: &UnboundedSender<GuiEvent>) {
    _ = events.send(GuiEvent::Notice(debug_info_text(agent)));
}

/// `/debug tools`：回执本次请求真正声明的工具提示词。
fn show_debug_tools(agent: &Agent, events: &UnboundedSender<GuiEvent>) {
    _ = events.send(GuiEvent::Notice(debug_tools_text(agent)));
}

/// `/debug context`：按来源拆开上下文窗口占用后回执。
fn show_context_breakdown(agent: &Agent, events: &UnboundedSender<GuiEvent>) {
    let tools: Vec<(String, String, serde_json::Value)> = agent
        .tools
        .iter()
        .filter(|(name, _, _)| !agent.hidden_declarations.contains(name))
        .cloned()
        .collect();
    let window = agent
        .routed_model
        .as_ref()
        .unwrap_or(&agent.model)
        .context_window;
    let breakdown =
        context_breakdown::breakdown(&agent.system_prompt, &tools, &agent.messages, window);
    _ = events.send(GuiEvent::Notice(context_breakdown::render(&breakdown)));
}

/// 设置/清除条目标签并回执结果（无会话时给一行提示）。
fn set_label(
    agent: &mut Agent,
    entry_id: &str,
    label: Option<String>,
    events: &UnboundedSender<GuiEvent>,
) {
    let message = match agent.session.as_mut() {
        Some(session) => match session.append_label_change(entry_id, label.as_deref()) {
            Ok(_) => match label {
                Some(text) => format!("Labeled {entry_id}: {text}"),
                None => format!("Cleared label on {entry_id}"),
            },
            Err(error) => format!("Label failed: {error}"),
        },
        None => "No current session".to_string(),
    };
    _ = events.send(GuiEvent::Notice(message));
}

/// `/share`：导出当前会话并上传成私密 gist，回执结果或失败原因。
fn share_session(agent: &Agent, theme: Option<String>, events: &UnboundedSender<GuiEvent>) {
    let message = match agent.session.as_ref() {
        Some(session) => match settings_manager::read_settings_session_share_destination() {
            settings_manager::SessionShareDestination::Radius => {
                "Share failed: Radius upload is not supported; set sessionShareDestination to \"gist\"".to_string()
            }
            _ => {
                let session_file = session
                    .get_session_file()
                    .map(|p| p.to_string_lossy().to_string());
                share_via_gist(
                    session,
                    theme.as_deref(),
                    agent.export_extras(session_file.as_deref()),
                )
            }
        },
        None => "Share failed: no current session".to_string(),
    };
    _ = events.send(GuiEvent::Notice(message));
}

/// `/export [out]`：把当前会话导出成 HTML（含系统提示词与工具声明），回执结果。
fn export_session(
    agent: &Agent,
    out: Option<String>,
    theme: Option<String>,
    events: &UnboundedSender<GuiEvent>,
) {
    let message = match agent.session.as_ref() {
        Some(session) => {
            let extras = agent.export_extras(None);
            match export_html::export_session_extra(
                session,
                out.as_deref(),
                theme.as_deref(),
                Some(extras),
            ) {
                Ok(path) => format!("Exported: {path}"),
                Err(error) => format!("Export failed: {error}"),
            }
        }
        None => "No current session to export".to_string(),
    };
    _ = events.send(GuiEvent::Notice(message));
}

/// 登录后立刻用新 key（同 TUI）：当前模型就是这个 provider 时直接换 key。
fn apply_key(agent: &mut Agent, provider: &str, key: String) {
    if agent.model.provider == provider {
        agent.model.api_key = key;
    }
}

/// 扩展 / 信任决策变了要重建工具，否则下一轮还是旧工具集。
fn rebuild_tools(agent: &mut Agent) {
    agent.rebuild_tools();
}

/// 扩展持久化（`PersistSessionEntry`）：追加自定义条目并回执，扩展据此知道写盘成功。
fn append_custom_entry(agent: &mut Agent, custom_type: &str, data: Option<serde_json::Value>) {
    let id = agent
        .session
        .as_mut()
        .map(|session| session.append_custom_entry(custom_type, data))
        .unwrap_or_default();
    agent.emit(serde_json::json!({
        "type": "extension:entry_persisted",
        "customType": custom_type,
        "entryId": id
    }));
}

/// `/rewind`：回退最后一条用户输入。
///
/// 回退会改 `agent.messages`，所以推 [`GuiEvent::Rewound`]（界面据此重建），
/// 否则界面上最后一条输入与它的输出会继续挂着。
fn rewind(agent: &mut Agent, events: &UnboundedSender<GuiEvent>) {
    match agent.rewind_last_user_input() {
        Ok(Some(removed_input)) => {
            _ = events.send(GuiEvent::Rewound {
                messages: agent.messages.clone(),
                removed_input,
            });
        }
        Ok(None) => {
            _ = events.send(GuiEvent::Notice(
                "Nothing to rewind: no user input in the current session.".to_string(),
            ));
        }
        Err(e) => _ = events.send(GuiEvent::Error(e.to_string())),
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

/// `/reload`：重读扩展启用状态、上下文文件、技能、提示词模板与系统提示，再重建工具。
///
/// `--skill` / `--prompt-template` / `--no-context-files` 这类启动参数取自
/// [`ReloadCtx`]（启动时那一份），重载后不回到默认。
fn reload_assets(
    agent: &mut Agent,
    events: &UnboundedSender<GuiEvent>,
    reload_ctx: &ReloadCtx,
    templates: &mut Vec<PromptTemplate>,
) {
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
        &reload_ctx.skill_paths,
        &bundled_skill_names(),
        reload_ctx.include_default_skills,
        trusted,
        &settings.skills,
    );

    let files = if reload_ctx.no_context_files {
        Vec::new()
    } else {
        core::context::load_project_context_files(&cwd, &agent_dir)
    };
    agent.rebuild_ctx.context_files = if trusted {
        files
    } else {
        files
            .into_iter()
            .filter(|(path, _)| Path::new(path).starts_with(&agent_dir))
            .collect()
    };

    // 系统提示也要重发现：改了 SYSTEM.md / APPEND_SYSTEM.md 之后 `/reload` 得生效，
    // 规则与 TUI 一致（SYSTEM.md 优先于 --system-prompt，--append-system-prompt 优先于发现）
    agent.rebuild_ctx.system_prompt =
        core::context::discover_system_prompt(&cwd, &agent_dir, trusted)
            .filter(|prompt| !prompt.trim().is_empty())
            .or_else(|| reload_ctx.cli_system_prompt.clone());
    agent.rebuild_ctx.append_system_prompt = if reload_ctx.cli_append_system_prompt.is_some() {
        reload_ctx.cli_append_system_prompt.clone()
    } else {
        core::context::discover_append_system_prompt(&cwd, &agent_dir, trusted)
    };

    // 提示词模板：`--prompt-template` 路径与默认发现开关沿用启动参数
    *templates = prompt_templates::load_prompt_templates(
        &cwd,
        &agent_dir,
        &reload_ctx.prompt_paths,
        reload_ctx.include_default_prompt_templates,
    );
    for warning in prompt_templates::take_load_warnings() {
        _ = events.send(GuiEvent::Notice(warning));
    }

    _ = events.send(GuiEvent::PromptAssets {
        skills: agent.skills.clone(),
        templates: templates.clone(),
    });

    agent.rebuild_tools();
    _ = events.send(GuiEvent::Notice(format!(
        "Reloaded: {} skills, {} context files, {} prompt templates",
        agent.skills.len(),
        agent.rebuild_ctx.context_files.len(),
        templates.len()
    )));
}

/// 未完成 operation 面板确认后的恢复：`continue` = 只读容忍装配；
/// `rewrite` = 先为旧 run 补 `operation_finished` 再装配。确认路径不再二次弹面板。
fn resume_zombie(agent: &mut Agent, path: &str, action: &str, events: &UnboundedSender<GuiEvent>) {
    let mut session = match Session::open_checked(path) {
        OpenSessionResult::Ok(session) => *session,
        OpenSessionResult::Invalid(error) => {
            _ = events.send(GuiEvent::Error(format!("Failed to open session: {error}")));
            return;
        }
        // 防御：确认后再读反而行损坏，转修复面板
        OpenSessionResult::Repairable { path, plan } => {
            _ = events.send(GuiEvent::SessionRepair { path, plan });
            return;
        }
    };

    let note = if action == "rewrite" {
        let squashed = session.squash_stale_operations();
        match session.take_persist_error() {
            Some(error) => format!(
                "Squashed {squashed} interrupted run(s), but the disk write failed: {error}"
            ),
            None if squashed == 0 => "No stale operations to squash".to_string(),
            None => {
                format!("Marked {squashed} interrupted run(s) as finished (session log updated)")
            }
        }
    } else {
        "Opening session with latest run; older interrupted runs left as-is".to_string()
    };

    adopt_session(agent, session, events, &note);
}

/// 把 fork/clone 出来的新会话接到 agent 上，并把历史推给 GUI。
/// 会话变了就重发一次切换：plan-mode / tasks / cache 计数等扩展靠它复位。
///
/// 新会话尚未落盘时 `path` 为 `None`（与 TUI 的 `/new` 一致）。
fn dispatch_session_switched(agent: &Agent) {
    let path = agent
        .session
        .as_ref()
        .and_then(|session| session.get_session_file())
        .map(|file| file.to_string_lossy().to_string());
    core::extensions::dispatch_session_switched(path.as_deref(), &agent.messages);

    // `/new` 的新会话尚未落盘：路径为空，但 id 已在会话里（见 `Extension::on_session_id`）。
    if let Some(session) = &agent.session {
        core::extensions::dispatch_session_id(&session.session_id);
    }
}

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
    dispatch_session_switched(agent);
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

/// 只换目录（`worktree` 用）：会话与历史都不动，只按新目录重建工具与系统提示。
fn set_cwd(agent: &mut Agent, path: &str, events: &UnboundedSender<GuiEvent>) {
    if !Path::new(path).is_dir() {
        _ = events.send(GuiEvent::Error(format!(
            "Working directory does not exist: {path}"
        )));
        return;
    }

    agent.cwd = path.to_string();
    agent.rebuild_tools();
    core::extensions::dispatch_exec_ctx(&agent.exec_ctx());
    _ = events.send(GuiEvent::Snapshot(Box::new(snapshot(agent))));
}

/// 开新会话：清空消息、建一个持久会话文件（与 TUI 的 `/new` 一致）。
///
/// 不预先建工程桶目录：首次写入才建，否则 `/session rm-*` 删完空目录又会长回来。
fn new_session(agent: &mut Agent, events: &UnboundedSender<GuiEvent>) {
    let cwd = agent.cwd.clone();
    let dir = session_manager::default_session_dir(&cwd, &settings_manager::agent_dir());

    match Session::create_deferred(&cwd, Some(dir)) {
        Ok(session) => {
            agent.messages.clear();
            agent.session = Some(session);
            core::extensions::dispatch_exec_ctx(&agent.exec_ctx());
            dispatch_session_switched(agent);
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
            // 崩溃残留的未完成 operation ≥2：先让用户选恢复方式，别直接装配
            let count = session.open_operations_count();
            if count >= 2 {
                _ = events.send(GuiEvent::SessionZombie {
                    path: path.into(),
                    count,
                });
                return;
            }

            agent.messages = session.build_context_messages();
            agent.session = Some(*session);
            agent.restore_session_tools();
            core::extensions::dispatch_exec_ctx(&agent.exec_ctx());
            dispatch_session_switched(agent);
            _ = events.send(GuiEvent::History(agent.messages.clone()));
        }
        OpenSessionResult::Invalid(e) => {
            _ = events.send(GuiEvent::Error(format!("Cannot open session: {e}")));
        }
        // 中间行损坏：交给界面弹修复面板（抢救 / 截断 / 取消），修好后再开
        OpenSessionResult::Repairable { path, plan } => {
            _ = events.send(GuiEvent::SessionRepair { path, plan });
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
            } => {
                let result = agent.navigate_tree(&target_id, summarize).await.map(|_| ());
                // 导航换了活动分支：对话区必须按新分支重建，否则界面上还是旧分支的内容
                if result.is_ok() {
                    _ = events.send(GuiEvent::History(agent.messages.clone()));
                }
                result
            }
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
        session_name: agent.session.as_ref().and_then(|s| s.name.clone()),
        messages: agent.messages.len(),
        tools: agent.get_active_tools(),
        cwd: agent.cwd.clone(),
        usage,
    }
}
