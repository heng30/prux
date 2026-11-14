//! RPC 服务端主循环：读 stdin 的命令 → 分发 → 把 agent 事件原样写回 stdout。
//!
//! # 为什么只有回合与压缩会占住命令流
//!
//! `prompt` 要跑到整轮（含工具循环）结束，`compact` 要等一次摘要请求回来；这两件事
//! 独占 `&mut Agent`。所以它们各自起一个「长任务」，期间主循环照常读 stdin，但只接
//! **不需要 Agent 的命令**（`steer` / `follow_up` / `abort` / `clear_queue` / `abort_bash`）：
//! 这些走的都是与 Agent 共享的 `Arc`，不需要借它。其余命令直接回一句「正在处理」——
//! 这比让客户端干等一个回合诚实，也不会在长任务与命令之间造出第二套状态。
//!
//! 回合结算后排空 follow-up 队列接着跑，与 pi 的「settle 后再发」一致；
//! 扩展 UI 请求由独立任务轮询转成 `extension_ui_request` 记录，客户端答回来再唤醒发起方。

use super::{host::RpcHost, protocol};
use crate::{
    APP_NAME,
    core::{
        agent_session::Agent,
        auth::flush_oauth_refreshes,
        compaction::{CompactionReason, estimate_context_tokens},
        extensions::{
            self, ExtensionUiRequest, FormAnswer, FormRequest, FormResponse, OverlayEvent,
            SelectOption, UiNotifyLevel,
        },
        model_resolver::{self, CatalogModel},
        prompt_templates::{self, PromptTemplate},
        provider::{AgentMessage, ContentBlock, ModelConfig, ModelType, Usage},
        session_manager::{self, Session, SessionCreateOptions},
        settings_manager, skills,
        skills::Skill,
        tools::{self, BashSessionEnv},
    },
};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    future::Future,
    pin::pin,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    sync::mpsc,
    task::JoinHandle,
};

/// 入站记录流。
type Commands = mpsc::UnboundedReceiver<Inbound>;

/// 无会话文件时的协议级会话标识（`--no-session`）。
const EPHEMERAL_SESSION_ID: &str = "prux-rpc";

/// 扩展 UI 队列的轮询间隔：扩展发请求没有回调，只能定时取。
const UI_POLL: Duration = Duration::from_millis(50);

/// 一条入站记录：客户端命令，或 stdin 关闭。
enum Inbound {
    /// 客户端写在 stdin 上的一行 JSON。
    Command(Value),
    /// stdin 已关闭（进程该收尾了）。
    Closed,
}

/// 当前由 RPC 起的 bash 进程 id（`abort_bash` 据此杀进程）；空闲时为 `None`。
fn user_bash() -> &'static Mutex<Option<String>> {
    /// 运行中的 RPC bash 进程 id 槽（叶级锁）。
    static B: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    B.get_or_init(Default::default)
}

/// RPC 起的 bash 进程 id 计数器：同一条流上可以同时有几条 `bash` 在跑，id 不能撞。
fn next_bash_id() -> String {
    /// bash 进程 id 序号（进程内自增）。
    static N: AtomicU64 = AtomicU64::new(1);
    format!("rpc-bash-{}", N.fetch_add(1, Ordering::Relaxed))
}

/// 在跑的 bash 任务句柄：关停前要把它们都等完，否则客户端只看到 stdin 被关、丢了响应。
fn bash_tasks() -> &'static Mutex<Vec<JoinHandle<()>>> {
    /// bash 任务句柄表（叶级锁）：投新任务时清掉已结束的，关停时统一 await 剩下的。
    static T: OnceLock<Mutex<Vec<JoinHandle<()>>>> = OnceLock::new();
    T.get_or_init(Default::default)
}

/// 主循环状态：与 Agent 分开持有，好让忙碌期的命令不必借 Agent。
pub(super) struct State {
    /// 当前回合的取消信号（与 Agent 共享同一个 Arc）。
    abort: Arc<AtomicBool>,
    /// 运行中 steer 注入箱（与 Agent 共享同一个 Arc）。
    steer: Arc<Mutex<VecDeque<AgentMessage>>>,
    /// settle 后才投递的 follow-up 队列（RPC 自己持有）。
    follow_ups: VecDeque<AgentMessage>,
    /// 启动时的技能快照：steer / follow-up 在忙碌期也要能展开 `/skill:名`。
    skills: Vec<Skill>,
    /// 启动时的提示词模板快照（`get_commands` 与 `/模板名` 展开共用）。
    templates: Vec<PromptTemplate>,
    /// 是否有长任务（回合 / 压缩）在跑。
    busy: bool,
    /// stdin 是否已关闭。
    input_closed: bool,
    /// 扩展命令要求退出（`/quit` 这类 handler 返回 true）。
    quit: bool,
    /// 忙碌期收到的宿主侧内部命令（要动 Agent，只能等长任务结束再执行）。
    pending_internal: VecDeque<Value>,
}

impl State {
    /// 从已装配好的 Agent 取共享句柄，并缓存技能与提示词模板。
    fn new(agent: &Agent) -> State {
        State {
            abort: agent.abort_handle(),
            steer: agent.runtime_steer_inbox.clone(),
            follow_ups: VecDeque::new(),
            skills: agent.skills.clone(),
            templates: prompt_templates::load_prompt_templates(
                &agent.cwd,
                &settings_manager::agent_dir(),
                &[],
                true,
            ),
            busy: false,
            input_closed: false,
            quit: false,
            pending_internal: VecDeque::new(),
        }
    }

    /// 展开 `/skill:名` 与 `/模板名`（与 TUI 提交路径同一条链）。
    fn expand(&self, text: &str) -> String {
        let expanded = skills::expand_skill_command(text, &self.skills);
        prompt_templates::expand_prompt_template(&expanded, &self.templates)
    }

    /// 排入运行中 steer 注入箱（下一轮请求前被 Agent 取走）。
    fn push_steer(&mut self, message: AgentMessage) {
        self.steer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(message);
    }

    /// 排入 follow-up 队列：本轮结算后再起一轮。
    pub(super) fn push_follow_up(&mut self, text: &str) {
        let text = self.expand(text);
        if !text.trim().is_empty() {
            self.follow_ups.push_back(AgentMessage::user_text(&text));
        }
    }

    /// 清空两条队列，返回被丢弃的文本（`clear_queue` 的回执）。
    fn clear(&mut self) -> (Vec<String>, Vec<String>) {
        let steering: Vec<String> = self
            .steer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
            .map(|m| m.text())
            .collect();
        let follow_up: Vec<String> = self.follow_ups.drain(..).map(|m| m.text()).collect();
        (steering, follow_up)
    }

    /// 尚未投递的消息条数（`get_state.pendingMessageCount`）。
    fn pending(&self) -> usize {
        self.follow_ups.len() + self.steer.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

/// 跑 RPC 服务端，直到 stdin 关闭。返回进程退出码。
pub(super) async fn run(agent: Agent) -> i32 {
    let mut agent = agent;
    agent.attach_json_sink(Box::new(|event| protocol::emit(&event)));
    extensions::set_ui_ready(true);
    install_form_host();

    let mut state = State::new(&agent);
    let (inbox, mut commands) = mpsc::unbounded_channel();
    spawn_reader(inbox.clone());
    spawn_ui_pump(inbox.clone());

    loop {
        let Some(inbound) = commands.recv().await else {
            break;
        };
        let command = match inbound {
            Inbound::Closed => {
                state.input_closed = true;
                break;
            }
            Inbound::Command(command) => command,
        };

        if let Some(response) = dispatch(&mut agent, &mut state, &command, &mut commands).await {
            protocol::emit(&response);
        }

        // 忙碌期攒下的宿主侧内部命令（如扩展要求的会话改名）在这里补做
        while let Some(internal) = state.pending_internal.pop_front() {
            if let Some(response) = dispatch(&mut agent, &mut state, &internal, &mut commands).await
            {
                protocol::emit(&response);
            }
        }

        if state.input_closed || state.quit {
            break;
        }
    }

    extensions::set_ui_ready(false);
    // 关停前等在途 bash 写完响应：stdin 关闭不该让客户端丢掉最后一条结果
    let running: Vec<_> = bash_tasks()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .drain(..)
        .collect();

    for task in running {
        _ = task.await;
    }

    // 再等在途 OAuth 刷新落盘，别让轮转后的 token 丢掉
    flush_oauth_refreshes(Duration::from_secs(5)).await;
    0
}

/// 起一个读 stdin 的任务：解析成功的命令进 channel，`extension_ui_response` 直接分发。
fn spawn_reader(tx: mpsc::UnboundedSender<Inbound>) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(tokio::io::stdin()).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    if line.trim().is_empty() {
                        continue;
                    }
                    let Ok(command) = serde_json::from_str::<Value>(&line) else {
                        protocol::emit(&protocol::failure(
                            None,
                            "parse",
                            "Failed to parse command: invalid JSON",
                        ));
                        continue;
                    };
                    if command.get("type").and_then(Value::as_str) == Some("extension_ui_response")
                    {
                        if let Some(id) = command.get("id").and_then(Value::as_str) {
                            protocol::complete(id, command.clone());
                        }
                        continue;
                    }
                    if tx.send(Inbound::Command(command)).is_err() {
                        return;
                    }
                }
                Ok(None) => {
                    _ = tx.send(Inbound::Closed);
                    return;
                }
                Err(e) => {
                    eprintln!("{APP_NAME}: rpc stdin error: {e}");
                    _ = tx.send(Inbound::Closed);
                    return;
                }
            }
        }
    });
}

/// 起扩展 UI 泵：把扩展的 UI 请求翻成 `extension_ui_request`，答复经回传通道送回。
fn spawn_ui_pump(inbox: mpsc::UnboundedSender<Inbound>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(UI_POLL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            while let Some(request) = extensions::take_pending_ui() {
                match request {
                    ExtensionUiRequest::Select { id, title, options } => {
                        let choice = pick(&title, &options).await;
                        extensions::submit_ui_choice(id, choice);
                    }
                    ExtensionUiRequest::Notify { text, level } => notify(&text, level),
                    ExtensionUiRequest::NotifyRich { spans, level } => {
                        let text: String = spans.into_iter().map(|s| s.text).collect();
                        notify(&text, level);
                    }
                    ExtensionUiRequest::Status { text }
                    | ExtensionUiRequest::Toast { text, .. } => protocol::fire(
                        "setStatus",
                        json!({"statusKey": "status", "statusText": text}),
                    ),
                    // 覆盖层没人画也没人按键：直接送 Closed，否则扩展的调用会永远等下去。
                    ExtensionUiRequest::ShowOverlay { id } => {
                        extensions::overlay_event(id, OverlayEvent::Closed);
                    }
                    // 会话改名要动 Agent：转成一条宿主侧命令，由主循环在空闲分支处理。
                    ExtensionUiRequest::SetSessionName { name } => {
                        _ = inbox.send(Inbound::Command(json!({
                            "type": "__set_session_name",
                            "name": name,
                        })));
                    }
                    // 停靠面板 / 设置面板 / 自定义卡片 / 续跑意图都是 TUI 专属，丢弃不阻塞调用方。
                    _ => {}
                }
            }
        }
    });
}

/// 注册表单宿主：`ask_user_question` 的问卷逐题翻成 `select` 请求。
///
/// 协议里的 `select` 只能回一个选项，多选与自由作答都表达不了——多选跳过（调用方会把它
/// 列为未作答），备注里说清是哪几题，模型据此再文本补问。
fn install_form_host() {
    extensions::set_form_host(Some(Arc::new(|request: FormRequest| {
        Box::pin(async move {
            let FormRequest { message, questions } = request;
            let mut answers = Vec::with_capacity(questions.len());
            let mut skipped = Vec::new();

            for question in &questions {
                if question.multi {
                    skipped.push(question.question.clone());
                    answers.push(FormAnswer::default());
                    continue;
                }
                let options: Vec<SelectOption> = question
                    .options
                    .iter()
                    .map(|option| SelectOption::new(option.label.clone()))
                    .collect();
                let title = if message.is_empty() {
                    question.question.clone()
                } else {
                    format!("{message}\n{}", question.question)
                };
                let picked = pick(&title, &options).await?;
                answers.push(FormAnswer {
                    picked: vec![picked],
                    custom: String::new(),
                });
            }

            Some(FormResponse {
                answers,
                note: if skipped.is_empty() {
                    String::new()
                } else {
                    format!("client cannot render multi-select: {}", skipped.join("; "))
                },
            })
        })
    })));
}

/// 发一条 `select` 并等客户端选；取消 / 回包认不出时返回 `None`。
async fn pick(title: &str, options: &[SelectOption]) -> Option<String> {
    let labels: Vec<String> = options.iter().map(|o| o.text.clone()).collect();
    let response = protocol::ask("select", json!({"title": title, "options": labels})).await?;
    if response.get("cancelled").is_some() {
        return None;
    }
    let value = response.get("value").and_then(Value::as_str)?;
    // 客户端可以回选中文本，也可以回下标；两样都认
    options
        .iter()
        .find(|option| option.text == value)
        .map(|option| option.text.clone())
        .or_else(|| {
            value
                .parse()
                .ok()
                .and_then(|index: usize| labels.get(index).cloned())
        })
}

/// 发一条 `notify`（fire-and-forget）。
fn notify(text: &str, level: UiNotifyLevel) {
    let kind = match level {
        UiNotifyLevel::Error => "error",
        UiNotifyLevel::Warning => "warning",
        UiNotifyLevel::Info | UiNotifyLevel::Success => "info",
    };
    protocol::fire("notify", json!({"message": text, "notifyType": kind}));
}

/// 长任务：一个回合，或一次手动压缩。
enum Long {
    /// 要投递的用户消息。
    Turn(Box<AgentMessage>),
    /// 带自定义摘要指令的手动压缩；`None` = 不带。
    Compact(Option<String>),
}

/// 跑长任务，期间继续消费「忙碌也可安全执行」的命令；返回压缩结果（回合返回 `None`）。
async fn run_long(
    agent: &mut Agent,
    mut op: Long,
    commands: &mut Commands,
    state: &mut State,
) -> Option<Value> {
    state.busy = true;
    let mut compact_result = None;
    loop {
        let is_turn = match op {
            Long::Turn(message) => {
                match busy_wait(agent.prompt_messages(vec![*message]), commands, state).await {
                    Ok(_) => {}
                    Err(e) => eprintln!("{APP_NAME}: rpc prompt failed: {e}"),
                }
                true
            }
            Long::Compact(instructions) => {
                let compact =
                    agent.maybe_compact_with(CompactionReason::Manual, instructions.as_deref());
                match busy_wait(compact, commands, state).await {
                    Ok(result) => compact_result = result,
                    Err(e) => eprintln!("{APP_NAME}: rpc compact failed: {e}"),
                }
                false
            }
        };

        // 回合结算后接着跑队列里的 follow-up（pi 的「settle 后再发」语义）
        match state.follow_ups.pop_front() {
            Some(message) if is_turn => op = Long::Turn(Box::new(message)),
            _ => break,
        }
    }
    state.busy = false;
    compact_result
}

/// 等长任务结束，期间把入站命令交给忙碌期处理器。
async fn busy_wait<F: Future>(future: F, commands: &mut Commands, state: &mut State) -> F::Output {
    let mut future = pin!(future);
    loop {
        let closed = state.input_closed;
        tokio::select! {
            output = &mut future => return output,
            inbound = commands.recv(), if !closed => match inbound {
                Some(Inbound::Command(command)) => {
                    if let Some(response) = handle_busy(state, &command) {
                        protocol::emit(&response);
                    }
                }
                // stdin 关闭：让本轮跑完（收尾里还有会话落盘与 token 刷新）
                Some(Inbound::Closed) | None => state.input_closed = true,
            },
        }
    }
}

/// 忙碌期能处理的命令：只碰与 Agent 共享的 `Arc`，不借 `&mut Agent`。
///
/// 队列类与中止类命令在空闲期是同一份实现（[`cmd_queue_input`] / [`cmd_abort`] 等），
/// 这里只多判一件事：忙着的 `prompt` 必须给 `streamingBehavior` 才知道往哪条队列排。
fn handle_busy(state: &mut State, command: &Value) -> Option<Value> {
    let (id, kind) = (command_id(command), command_type(command));

    match kind {
        "prompt" => {
            if let Some(error) = queued_command_error(str_field(command, "message")) {
                return Some(protocol::failure(id, kind, &error));
            }
            match command.get("streamingBehavior").and_then(Value::as_str) {
                Some("steer") => {
                    let expanded = state.expand(str_field(command, "message"));
                    state.push_steer(user_message(&expanded, command));
                    Some(protocol::success(
                        id,
                        kind,
                        Some(json!({"disposition": "queued"})),
                    ))
                }
                Some("followUp") => {
                    state.push_follow_up(str_field(command, "message"));
                    Some(protocol::success(
                        id,
                        kind,
                        Some(json!({"disposition": "queued"})),
                    ))
                }
                _ => Some(protocol::failure(
                    id,
                    kind,
                    "Agent is already processing. Specify streamingBehavior ('steer' or 'followUp') to queue the message.",
                )),
            }
        }
        "steer" | "follow_up" => cmd_queue_input(state, command),
        "abort" => cmd_abort(state, command),
        "clear_queue" => cmd_clear_queue(state, command),
        "abort_bash" => cmd_abort_bash(command),
        // 宿主侧内部命令要动 Agent：先攒着，长任务结束后由主循环补做，不给客户端任何回执
        internal if internal.starts_with("__") => {
            state.pending_internal.push_back(command.clone());
            None
        }
        other => Some(protocol::failure(
            id,
            other,
            &format!(
                "Agent is already processing; `{other}` is unavailable until the run settles."
            ),
        )),
    }
}

/// 命令类型（`type` 字段）；缺失时为空串，最终落到「未知命令」。
fn command_type(command: &Value) -> &str {
    command
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

/// 命令标识：客户端给了就原样带回响应，没给就不带。
fn command_id(command: &Value) -> Option<&Value> {
    command.get("id")
}

/// 命令的字符串字段；缺失或类型不对时为空串。
fn str_field<'a>(command: &'a Value, key: &str) -> &'a str {
    command.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// 空闲期分发一条命令；返回要写回 stdout 的响应（`None` 表示已自行写过）。
async fn dispatch(
    agent: &mut Agent,
    state: &mut State,
    command: &Value,
    commands: &mut Commands,
) -> Option<Value> {
    match command_type(command) {
        // ---- 提问 ----
        "prompt" => cmd_prompt(agent, state, command, commands).await,
        "steer" | "follow_up" => cmd_queue_input(state, command),
        "abort" => cmd_abort(state, command),
        "clear_queue" => cmd_clear_queue(state, command),
        "new_session" => cmd_new_session(agent, command),
        // ---- 状态 ----
        "get_state" => cmd_get_state(agent, state, command),
        "get_messages" => cmd_get_messages(agent, command),
        // ---- 模型 ----
        "set_model" => cmd_set_model(agent, command),
        "cycle_model" => cmd_cycle_model(agent, command),
        "get_available_models" => cmd_get_available_models(command),
        // ---- 思考档位 ----
        "set_thinking_level" => cmd_set_thinking_level(agent, command),
        "cycle_thinking_level" => cmd_cycle_thinking_level(agent, command),
        "get_available_thinking_levels" => cmd_get_available_thinking_levels(agent, command),
        // ---- 队列模式 ----
        "set_steering_mode" | "set_follow_up_mode" => cmd_set_queue_mode(agent, command),
        // ---- 压缩 ----
        "compact" => cmd_compact(agent, state, command, commands).await,
        "set_auto_compaction" => cmd_set_auto_compaction(agent, command),
        // ---- 重试 ----
        "set_auto_retry" => cmd_set_auto_retry(agent, command),
        "abort_retry" => cmd_abort_retry(command),
        // ---- bash ----
        "bash" => cmd_bash(agent, command),
        "abort_bash" => cmd_abort_bash(command),
        // ---- 会话 ----
        "get_session_stats" => cmd_get_session_stats(agent, command),
        "export_html" => cmd_export_html(agent, command),
        "switch_session" => cmd_switch_session(agent, command),
        "fork" | "clone" => cmd_fork(agent, command),
        "get_fork_messages" => cmd_get_fork_messages(agent, command),
        "get_entries" => cmd_get_entries(agent, command),
        "get_tree" => cmd_get_tree(agent, command),
        "get_last_assistant_text" => cmd_get_last_assistant_text(agent, command),
        "set_session_name" | "__set_session_name" => cmd_set_session_name(agent, command),
        // ---- 命令表 ----
        "get_commands" => cmd_get_commands(state, command),

        other => Some(protocol::failure(
            command_id(command),
            other,
            &format!("Unknown command: {other}"),
        )),
    }
}

/// `prompt`：以 `/` 开头的文本先试扩展斜杠命令，命中就地执行并回 `handled`；
/// 否则回 `started` 并开一轮（响应先发，结果从事件流读）。
async fn cmd_prompt(
    agent: &mut Agent,
    state: &mut State,
    command: &Value,
    commands: &mut Commands,
) -> Option<Value> {
    let (id, message) = (command_id(command), str_field(command, "message"));
    if message.trim().is_empty() {
        return Some(protocol::failure(id, "prompt", "message is required"));
    }

    if message.starts_with('/') && dispatch_slash(agent, state, message) {
        // 命令自己可能追加了一条用户消息（如 `/compact` 之后的续跑意图）
        if let Some(next) = state.follow_ups.pop_front() {
            run_long(agent, Long::Turn(Box::new(next)), commands, state).await;
        }
        return Some(protocol::success(
            id,
            "prompt",
            Some(json!({"disposition": "handled"})),
        ));
    }

    let user = user_message(&state.expand(message), command);
    protocol::emit(&protocol::success(
        id,
        "prompt",
        Some(json!({"disposition": "started"})),
    ));
    run_long(agent, Long::Turn(Box::new(user)), commands, state).await;
    None
}

/// `steer` / `follow_up`：排队一条用户消息（steer 进运行中注入箱，follow-up 等结算）。
///
/// 扩展斜杠命令不能排队——它要的是当场执行，回错并指路 `prompt`。
fn cmd_queue_input(state: &mut State, command: &Value) -> Option<Value> {
    let (id, kind) = (command_id(command), command_type(command));
    let message = str_field(command, "message");
    if message.trim().is_empty() {
        return Some(protocol::failure(id, kind, "message is required"));
    }
    if let Some(error) = queued_command_error(message) {
        return Some(protocol::failure(id, kind, &error));
    }

    let expanded = state.expand(message);
    if kind == "steer" {
        state.push_steer(user_message(&expanded, command));
    } else {
        state
            .follow_ups
            .push_back(AgentMessage::user_text(&expanded));
    }
    Some(protocol::success(
        id,
        kind,
        Some(json!({"disposition": "queued"})),
    ))
}

/// `abort`：置位本轮取消信号（空闲时置位无害，下一次 run 会复位）。
fn cmd_abort(state: &mut State, command: &Value) -> Option<Value> {
    state.abort.store(true, Ordering::Relaxed);
    Some(protocol::success(command_id(command), "abort", None))
}

/// `clear_queue`：清空 steer 与 follow-up 两条队列，回执里带上被丢弃的文本。
fn cmd_clear_queue(state: &mut State, command: &Value) -> Option<Value> {
    let (steering, follow_up) = state.clear();
    Some(protocol::success(
        command_id(command),
        "clear_queue",
        Some(json!({"steering": steering, "followUp": follow_up})),
    ))
}

/// `new_session`：在当前目录开一个尚未落盘的新会话，可选挂到父会话下。
fn cmd_new_session(agent: &mut Agent, command: &Value) -> Option<Value> {
    let (id, kind) = (command_id(command), "new_session");
    let parent = command
        .get("parentSession")
        .and_then(Value::as_str)
        .map(str::to_string);
    match new_session(agent, parent) {
        Ok(()) => Some(protocol::success(
            id,
            kind,
            Some(json!({"cancelled": false})),
        )),
        Err(e) => Some(protocol::failure(id, kind, &e)),
    }
}

/// `get_state`：模型、档位、队列模式、会话标识与规模。
fn cmd_get_state(agent: &Agent, state: &State, command: &Value) -> Option<Value> {
    Some(protocol::success(
        command_id(command),
        "get_state",
        Some(session_state(agent, state)),
    ))
}

/// `get_messages`：当前上下文里的消息（含图片块原样）。
fn cmd_get_messages(agent: &Agent, command: &Value) -> Option<Value> {
    Some(protocol::success(
        command_id(command),
        "get_messages",
        Some(json!({ "messages": agent.messages })),
    ))
}

/// `set_model`：先按可用目录校验（与 pi 同口径），再切模型并写进 settings.json 的默认模型。
fn cmd_set_model(agent: &mut Agent, command: &Value) -> Option<Value> {
    let id = command_id(command);
    let provider = str_field(command, "provider");
    let model_id = str_field(command, "modelId");
    if !available_model(provider, model_id) {
        return Some(protocol::failure(
            id,
            "set_model",
            &format!("Model not found: {provider}/{model_id}"),
        ));
    }
    match agent.switch_model(provider, model_id, true) {
        Ok(()) => Some(protocol::success(
            id,
            "set_model",
            Some(model_json(&agent.model)),
        )),
        Err(e) => Some(protocol::failure(id, "set_model", &e.to_string())),
    }
}

/// `cycle_model`：按 scope（或全部可用模型）循环到下一个；只剩一个模型时回 `null`。
fn cmd_cycle_model(agent: &mut Agent, command: &Value) -> Option<Value> {
    Some(protocol::success(
        command_id(command),
        "cycle_model",
        Some(cycle_model(agent)),
    ))
}

/// `get_available_models`：凭据已配置的全部 chat 模型。
fn cmd_get_available_models(command: &Value) -> Option<Value> {
    Some(protocol::success(
        command_id(command),
        "get_available_models",
        Some(json!({ "models": available_models() })),
    ))
}

/// `set_thinking_level`：按模型能力钳制后写入会话档位。
fn cmd_set_thinking_level(agent: &mut Agent, command: &Value) -> Option<Value> {
    let id = command_id(command);
    let level = str_field(command, "level");
    if level.is_empty() {
        return Some(protocol::failure(
            id,
            "set_thinking_level",
            "level is required",
        ));
    }
    agent.set_thinking_level(level);
    Some(protocol::success(id, "set_thinking_level", None))
}

/// `cycle_thinking_level`：在当前模型支持的档位里往下走一格；不足两档时回 `null`。
fn cmd_cycle_thinking_level(agent: &mut Agent, command: &Value) -> Option<Value> {
    let id = command_id(command);
    let levels = agent.get_available_thinking_levels();
    if levels.len() < 2 {
        return Some(protocol::success(
            id,
            "cycle_thinking_level",
            Some(Value::Null),
        ));
    }
    let current = agent
        .thinking_level
        .clone()
        .unwrap_or_else(|| "off".to_string());
    let index = levels.iter().position(|l| *l == current).unwrap_or(0);
    let next = levels[(index + 1) % levels.len()];
    let effective = agent.set_thinking_level_silent(next, false);
    Some(protocol::success(
        id,
        "cycle_thinking_level",
        Some(json!({ "level": effective })),
    ))
}

/// `get_available_thinking_levels`：当前模型支持的档位（含 `off`）。
fn cmd_get_available_thinking_levels(agent: &Agent, command: &Value) -> Option<Value> {
    Some(protocol::success(
        command_id(command),
        "get_available_thinking_levels",
        Some(json!({ "levels": agent.get_available_thinking_levels() })),
    ))
}

/// `set_steering_mode` / `set_follow_up_mode`：改队列取出方式并写回 settings.json。
fn cmd_set_queue_mode(agent: &mut Agent, command: &Value) -> Option<Value> {
    let (id, kind) = (command_id(command), command_type(command));
    let mode = str_field(command, "mode");
    if mode != "all" && mode != "one-at-a-time" {
        return Some(protocol::failure(
            id,
            kind,
            "mode must be \"all\" or \"one-at-a-time\"",
        ));
    }
    if kind == "set_steering_mode" {
        agent.steer_mode = mode.to_string();
        _ = settings_manager::write_settings_steering_mode(mode);
    } else {
        agent.follow_up_mode = mode.to_string();
        _ = settings_manager::write_settings_follow_up_mode(mode);
    }
    Some(protocol::success(id, kind, None))
}

/// `compact`：手动压缩一轮，回压缩结果（没压成时为 `null`）。
async fn cmd_compact(
    agent: &mut Agent,
    state: &mut State,
    command: &Value,
    commands: &mut Commands,
) -> Option<Value> {
    let instructions = command
        .get("customInstructions")
        .and_then(Value::as_str)
        .map(str::to_string);
    let result = run_long(agent, Long::Compact(instructions), commands, state).await;
    Some(protocol::success(
        command_id(command),
        "compact",
        Some(result.unwrap_or(Value::Null)),
    ))
}

/// `set_auto_compaction`：切换阈值自动压缩并写回 settings.json。
fn cmd_set_auto_compaction(agent: &mut Agent, command: &Value) -> Option<Value> {
    let enabled = command
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    agent.auto_compaction = enabled;
    _ = settings_manager::write_settings_auto_compact(enabled);
    Some(protocol::success(
        command_id(command),
        "set_auto_compaction",
        None,
    ))
}

/// `set_auto_retry`：切换自动重试并写回 settings.json。
fn cmd_set_auto_retry(agent: &mut Agent, command: &Value) -> Option<Value> {
    let enabled = command
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    agent.auto_retry = enabled;
    _ = settings_manager::write_settings_retry_enabled(enabled);
    Some(protocol::success(
        command_id(command),
        "set_auto_retry",
        None,
    ))
}

/// `abort_retry`：幂等空操作——prux 的重试退避在回合内部，`abort` 已经把整轮停掉了。
fn cmd_abort_retry(command: &Value) -> Option<Value> {
    Some(protocol::success(command_id(command), "abort_retry", None))
}

/// `bash`：起一个不占命令流的 bash 任务，结果自己回来（响应不在本函数里写）。
fn cmd_bash(agent: &Agent, command: &Value) -> Option<Value> {
    spawn_bash(agent, command, command_id(command).cloned());
    None
}

/// `abort_bash`：杀掉当前由 RPC 起的 bash 进程。
fn cmd_abort_bash(command: &Value) -> Option<Value> {
    kill_user_bash();
    Some(protocol::success(command_id(command), "abort_bash", None))
}

/// `get_session_stats`：按整段会话历史统计条数与 token/费用。
fn cmd_get_session_stats(agent: &Agent, command: &Value) -> Option<Value> {
    Some(protocol::success(
        command_id(command),
        "get_session_stats",
        Some(session_stats(agent)),
    ))
}

/// `export_html`：把当前会话导出成 HTML，返回落地路径。
fn cmd_export_html(agent: &Agent, command: &Value) -> Option<Value> {
    let id = command_id(command);
    let output_path = command.get("outputPath").and_then(Value::as_str);
    let Some(session) = agent.session.as_ref() else {
        return Some(protocol::failure(id, "export_html", "no current session"));
    };
    let extras = agent.export_extras(None);
    match crate::core::export_html::export_session_extra(session, output_path, None, Some(extras)) {
        Ok(path) => Some(protocol::success(
            id,
            "export_html",
            Some(json!({ "path": path })),
        )),
        Err(e) => Some(protocol::failure(id, "export_html", &e.to_string())),
    }
}

/// `switch_session`：打开指定会话文件并接进 agent。
fn cmd_switch_session(agent: &mut Agent, command: &Value) -> Option<Value> {
    let id = command_id(command);
    match Session::open(str_field(command, "sessionPath")).map(|session| attach(agent, session)) {
        Ok(()) => Some(protocol::success(
            id,
            "switch_session",
            Some(json!({"cancelled": false})),
        )),
        Err(e) => Some(protocol::failure(id, "switch_session", &e.to_string())),
    }
}

/// `fork` / `clone`：从指定条目（fork）或当前叶子（clone）分叉出一条新会话并切过去。
fn cmd_fork(agent: &mut Agent, command: &Value) -> Option<Value> {
    let (id, kind) = (command_id(command), command_type(command));
    let entry_id = if kind == "fork" {
        Some(str_field(command, "entryId").to_string())
    } else {
        agent.session.as_ref().and_then(|s| s.clone_target_id())
    };
    let Some(entry_id) = entry_id.filter(|id| !id.is_empty()) else {
        let error = if kind == "fork" {
            "entryId is required"
        } else {
            "Cannot clone session: no current entry selected"
        };
        return Some(protocol::failure(id, kind, error));
    };
    let Some(path) = agent
        .session
        .as_ref()
        .and_then(|s| s.get_session_file())
        .map(|p| p.to_string_lossy().to_string())
    else {
        return Some(protocol::failure(id, kind, "no current session"));
    };

    let selected = entry_text(agent, &entry_id);
    match Session::fork_at(&path, &entry_id, &agent.cwd, None) {
        Ok(session) => {
            attach(agent, session);
            let data = if kind == "fork" {
                json!({"text": selected, "cancelled": false})
            } else {
                json!({"cancelled": false})
            };
            Some(protocol::success(id, kind, Some(data)))
        }
        Err(e) => Some(protocol::failure(id, kind, &e.to_string())),
    }
}

/// `get_fork_messages`：会话历史里可作为 fork 点的用户消息。
fn cmd_get_fork_messages(agent: &Agent, command: &Value) -> Option<Value> {
    Some(protocol::success(
        command_id(command),
        "get_fork_messages",
        Some(json!({ "messages": fork_messages(agent) })),
    ))
}

/// `get_entries`：全部条目（带 `since` 时只回它之后的部分）与当前叶子 id。
fn cmd_get_entries(agent: &Agent, command: &Value) -> Option<Value> {
    let id = command_id(command);
    let Some(session) = agent.session.as_ref() else {
        return Some(protocol::failure(id, "get_entries", "no current session"));
    };
    let entries = session.get_entries();
    let leaf = session.get_leaf_id().map(str::to_string);
    let entries = match command.get("since").and_then(Value::as_str) {
        Some(since) => {
            let Some(index) = entries
                .iter()
                .position(|e| e.get("id").and_then(Value::as_str) == Some(since))
            else {
                return Some(protocol::failure(
                    id,
                    "get_entries",
                    &format!("Entry not found: {since}"),
                ));
            };
            entries[index + 1..].to_vec()
        }
        None => entries,
    };
    Some(protocol::success(
        id,
        "get_entries",
        Some(json!({"entries": entries, "leafId": leaf})),
    ))
}

/// `get_tree`：会话分支树与当前叶子 id（形状由 [`Session::get_tree`] 决定）。
fn cmd_get_tree(agent: &Agent, command: &Value) -> Option<Value> {
    match agent.session.as_ref() {
        Some(session) => Some(protocol::success(
            command_id(command),
            "get_tree",
            Some(session.get_tree()),
        )),
        None => Some(protocol::failure(
            command_id(command),
            "get_tree",
            "no current session",
        )),
    }
}

/// `get_last_assistant_text`：最后一条 assistant 消息的正文；没有时为 `null`。
fn cmd_get_last_assistant_text(agent: &Agent, command: &Value) -> Option<Value> {
    let text = agent
        .messages
        .iter()
        .rev()
        .find(|m| m.role == "assistant")
        .map(|m| m.text());
    Some(protocol::success(
        command_id(command),
        "get_last_assistant_text",
        Some(json!({"text": text})),
    ))
}

/// `set_session_name`：给当前会话改名（空名字报错）；内部命令走同一条路径。
fn cmd_set_session_name(agent: &mut Agent, command: &Value) -> Option<Value> {
    let id = command_id(command);
    let name = str_field(command, "name").trim();
    if name.is_empty() {
        return Some(protocol::failure(
            id,
            "set_session_name",
            "Session name cannot be empty",
        ));
    }
    agent.set_session_name(name);
    Some(protocol::success(id, "set_session_name", None))
}

/// `get_commands`：可经 prompt 调用的扩展命令、提示词模板与技能。
fn cmd_get_commands(state: &State, command: &Value) -> Option<Value> {
    Some(protocol::success(
        command_id(command),
        "get_commands",
        Some(json!({ "commands": commands_list(state) })),
    ))
}

/// 执行一条斜杠命令；返回是否被扩展认领（`false` = 按普通文本处理）。
///
/// handler 的返回值在 TUI 里是「要不要退出」，这里同样记进 [`State::quit`]；
/// 只要扩展声明了这条命令就算认领——面板类命令在 RPC 里会回一句做不到，
/// 不能再把 `/worktree enter` 这种原文丢给模型当提示词。
fn dispatch_slash(agent: &mut Agent, state: &mut State, text: &str) -> bool {
    let name = slash_command_name(text);
    let Some(handler) = extensions::command_handler(name) else {
        return false;
    };
    let args = text
        .trim_start_matches('/')
        .split_once(char::is_whitespace)
        .map(|(_, args)| args.trim())
        .unwrap_or_default();
    let mut host = RpcHost {
        agent,
        state,
        nothing: (),
    };
    let quit = handler(&mut host, args);
    host.state.quit |= quit;
    true
}

/// 斜杠命令名（首个空白前的部分，去掉前导 `/`）。
fn slash_command_name(text: &str) -> &str {
    text.trim_start_matches('/')
        .split_whitespace()
        .next()
        .unwrap_or_default()
}

/// 排队类命令（steer / follow-up / 忙碌期 prompt）碰到扩展斜杠命令时的错误文本。
///
/// 斜杠命令要的是当场执行，塞进队列只会拖到下一轮才被当成普通文本发给模型。
fn queued_command_error(text: &str) -> Option<String> {
    is_extension_command(text).then(|| {
        format!(
            "Extension command \"/{}\" cannot be queued. Use prompt() or execute the command when not streaming.",
            slash_command_name(text)
        )
    })
}

/// `text` 是不是一条**已启用扩展声明过**的斜杠命令（steer / follow-up 不能排队它）。
fn is_extension_command(text: &str) -> bool {
    text.starts_with('/') && extensions::command_handler(slash_command_name(text)).is_some()
}

/// 中止当前 RPC bash 进程（没有在跑的命令时是空操作）。
fn kill_user_bash() {
    let running = user_bash().lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(running) = running {
        tools::abort_bash(&running);
    }
}

/// 起一个 bash 任务：不占主循环，结果回来后自己写响应。
fn spawn_bash(agent: &Agent, command: &Value, id: Option<Value>) {
    let line = command
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if line.trim().is_empty() {
        protocol::emit(&protocol::failure(
            id.as_ref(),
            "bash",
            "command is required",
        ));
        return;
    }
    let cwd = agent.cwd.clone();
    let env = bash_env(agent);
    let process_id = next_bash_id();
    *user_bash().lock().unwrap_or_else(|e| e.into_inner()) = Some(process_id.clone());

    let handle = tokio::spawn(async move {
        let outcome =
            tools::execute_bash_with_env(&line, None, &cwd, Some(&process_id), None, &env).await;
        let mut slot = user_bash().lock().unwrap_or_else(|e| e.into_inner());
        if slot.as_deref() == Some(process_id.as_str()) {
            *slot = None;
        }
        drop(slot);
        protocol::emit(&protocol::success(
            id.as_ref(),
            "bash",
            Some(json!({
                "output": outcome.text,
                "exitCode": outcome.exit_code,
                "cancelled": outcome.cancelled,
                "truncated": outcome.truncated,
                "fullOutputPath": outcome.full_output_path,
            })),
        ));
    });
    let mut tasks = bash_tasks().lock().unwrap_or_else(|e| e.into_inner());
    tasks.retain(|task| !task.is_finished());
    tasks.push(handle);
}

/// 交给 bash 的会话环境变量（session id/文件、provider/model、思考档位）。
fn bash_env(agent: &Agent) -> BashSessionEnv {
    BashSessionEnv {
        session_id: agent.session.as_ref().map(|s| s.session_id.clone()),
        session_file: agent
            .session
            .as_ref()
            .and_then(|s| s.get_session_file())
            .map(|p| p.to_string_lossy().to_string()),
        provider: Some(agent.model.provider.clone()),
        model: Some(agent.model.model_id.clone()),
        reasoning_level: agent.thinking_level.clone(),
        expose: settings_manager::read_settings_expose_session_environment(),
    }
}

/// 把打开的会话装配进 agent：重建消息上下文、恢复延迟工具、通知扩展。
fn attach(agent: &mut Agent, session: Session) {
    agent.messages = session.build_context_messages();
    agent.session = Some(session);
    agent.restore_session_tools();

    let file = agent
        .session
        .as_ref()
        .and_then(|s| s.get_session_file())
        .map(|p| p.to_string_lossy().to_string());
    extensions::dispatch_session_switched(file.as_deref(), &agent.messages);
    if let Some(session) = &agent.session {
        extensions::dispatch_session_id(&session.session_id);
    }
    extensions::dispatch_exec_ctx(&agent.exec_ctx());
}

/// 在当前目录新开一个会话（尚未落盘，首次写入才建文件）。
fn new_session(agent: &mut Agent, parent: Option<String>) -> Result<(), String> {
    let dir = session_manager::default_session_dir(&agent.cwd, &settings_manager::agent_dir());
    let session = Session::create_with(SessionCreateOptions {
        cwd: agent.cwd.clone(),
        session_dir: Some(dir),
        persist: true,
        parent_session_id: parent,
        defer_file: true,
        ..Default::default()
    })
    .map_err(|e| e.to_string())?;

    agent.messages.clear();
    agent.session = Some(session);
    extensions::dispatch_session_switched(None, &agent.messages);
    if let Some(session) = &agent.session {
        extensions::dispatch_session_id(&session.session_id);
    }
    extensions::dispatch_exec_ctx(&agent.exec_ctx());
    Ok(())
}

/// 一条用户消息：文本在前，附图作为 image 内容块追加在后。
fn user_message(text: &str, command: &Value) -> AgentMessage {
    let mut message = AgentMessage::user_text(text);
    for image in command
        .get("images")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let data = image
            .get("data")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let mime_type = image
            .get("mimeType")
            .and_then(Value::as_str)
            .unwrap_or("image/png");
        if !data.is_empty() {
            message.content.push(ContentBlock::Image {
                data: data.to_string(),
                mime_type: mime_type.to_string(),
            });
        }
    }
    message
}

/// 协议里的模型对象（只给标识与能力，**不含 api key**）。
fn model_json(config: &ModelConfig) -> Value {
    let name = model_resolver::list_models(&config.provider)
        .into_iter()
        .find(|(id, _)| id == &config.model_id)
        .map(|(_, name)| name)
        .unwrap_or_else(|| config.model_id.clone());
    json!({
        "provider": config.provider,
        "id": config.model_id,
        "name": name,
        "contextWindow": config.context_window,
        "reasoning": config.reasoning,
        "input": config.input,
    })
}

/// 凭据已配置的全部 chat 模型。
fn available_models() -> Vec<Value> {
    model_resolver::list_all_available_models_of_type(ModelType::Chat)
        .into_iter()
        .map(|model| {
            json!({
                "provider": model.provider,
                "id": model.id,
                "name": model.name,
            })
        })
        .collect()
}

/// 该模型是否在当前可用目录里（`set_model` 的前置校验，与 pi 同口径）。
fn available_model(provider: &str, model_id: &str) -> bool {
    model_resolver::list_all_available_models_of_type(ModelType::Chat)
        .into_iter()
        .any(|m| m.provider == provider && m.id == model_id)
}

/// 循环到下一个模型：scope（`--models` / settings.enabledModels）非空时严格按 scope 顺序。
fn cycle_model(agent: &mut Agent) -> Value {
    let available = model_resolver::list_all_available_models_of_type(ModelType::Chat);
    let scoped = agent.model_cycle.clone();
    let candidates: Vec<CatalogModel> = if scoped.is_empty() {
        available
    } else {
        scoped
            .iter()
            .filter_map(|s| {
                available
                    .iter()
                    .find(|m| m.provider == s.provider && m.id == s.id)
                    .cloned()
            })
            .collect()
    };
    if candidates.len() < 2 {
        return Value::Null;
    }

    let index = candidates
        .iter()
        .position(|m| m.provider == agent.model.provider && m.id == agent.model.model_id)
        .unwrap_or(0);
    let next = &candidates[(index + 1) % candidates.len()];

    // scope 条目带 thinking 时先同步档位（与 TUI 的 Ctrl+P 同语义）
    if let Some(scoped) = scoped
        .iter()
        .find(|s| s.provider == next.provider && s.id == next.id)
        && let Some(level) = &scoped.thinking
    {
        agent.set_thinking_level_silent(level, false);
    }
    if let Err(e) = agent.switch_model(&next.provider, &next.id, false) {
        return json!({ "error": e.to_string() });
    }
    json!({
        "model": model_json(&agent.model),
        "thinkingLevel": agent.thinking_level.clone().unwrap_or_else(|| "off".to_string()),
        "isScoped": !scoped.is_empty(),
    })
}

/// `get_state` 的数据：模型、档位、队列模式、会话标识与规模。
fn session_state(agent: &Agent, state: &State) -> Value {
    let session_file = agent
        .session
        .as_ref()
        .and_then(|s| s.get_session_file())
        .map(|p| p.to_string_lossy().to_string());
    let session_id = agent
        .session
        .as_ref()
        .map(|s| s.session_id.clone())
        .unwrap_or_else(|| EPHEMERAL_SESSION_ID.to_string());
    json!({
        "model": model_json(&agent.model),
        "thinkingLevel": agent.thinking_level.clone().unwrap_or_else(|| "off".to_string()),
        "isStreaming": state.busy,
        "isCompacting": agent.is_compacting,
        "steeringMode": agent.steer_mode,
        "followUpMode": agent.follow_up_mode,
        "sessionFile": session_file,
        "sessionId": session_id,
        "sessionName": agent.session.as_ref().and_then(|s| s.name.clone()),
        "autoCompactionEnabled": agent.auto_compaction,
        "messageCount": agent.messages.len(),
        "pendingMessageCount": state.pending(),
    })
}

/// 会话消息（整段历史；无会话文件时退回当前上下文）。
fn session_messages(agent: &Agent) -> Vec<Value> {
    match agent.session.as_ref() {
        Some(session) => session
            .get_entries()
            .into_iter()
            .filter(|e| e.get("type").and_then(Value::as_str) == Some("message"))
            .filter_map(|e| e.get("message").cloned())
            .collect(),
        None => serde_json::to_value(&agent.messages)
            .ok()
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default(),
    }
}

/// `get_session_stats`：按消息条数与 usage 汇总（含已压缩掉的历史）。
fn session_stats(agent: &Agent) -> Value {
    let mut user = 0u64;
    let mut assistant = 0u64;
    let mut tool_results = 0u64;
    let mut tool_calls = 0u64;
    let mut tokens = Usage::default();
    let mut cost = 0.0f64;

    let messages = session_messages(agent);
    for message in &messages {
        match message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "user" => user += 1,
            "assistant" => {
                assistant += 1;
                tool_calls += message
                    .get("content")
                    .and_then(Value::as_array)
                    .map(|blocks| {
                        blocks
                            .iter()
                            .filter(|b| {
                                b.get("type").and_then(Value::as_str) == Some("toolCall")
                                    || b.get("type").and_then(Value::as_str) == Some("tool_call")
                            })
                            .count() as u64
                    })
                    .unwrap_or(0);
            }
            "toolResult" => tool_results += 1,
            _ => {}
        }
        if let Some(usage) = message.get("usage")
            && let Ok(usage) = serde_json::from_value::<Usage>(usage.clone())
        {
            tokens.input += usage.input;
            tokens.output += usage.output;
            tokens.cache_read += usage.cache_read;
            tokens.cache_write += usage.cache_write;
            tokens.total_tokens += usage.total_tokens;
            cost += usage.cost.total;
        }
    }

    let estimate = estimate_context_tokens(&agent.messages);
    json!({
        "sessionFile": agent.session.as_ref().and_then(|s| s.get_session_file())
            .map(|p| p.to_string_lossy().to_string()),
        "sessionId": agent.session.as_ref().map(|s| s.session_id.clone())
            .unwrap_or_else(|| EPHEMERAL_SESSION_ID.to_string()),
        "userMessages": user,
        "assistantMessages": assistant,
        "toolCalls": tool_calls,
        "toolResults": tool_results,
        "totalMessages": messages.len(),
        "tokens": {
            "input": tokens.input,
            "output": tokens.output,
            "cacheRead": tokens.cache_read,
            "cacheWrite": tokens.cache_write,
            "total": tokens.total_tokens,
        },
        "cost": cost,
        "contextUsage": {
            "tokens": estimate.tokens,
            "contextWindow": agent.model.context_window,
            "percent": if agent.model.context_window == 0 {
                0.0
            } else {
                estimate.tokens as f64 / agent.model.context_window as f64 * 100.0
            },
        },
    })
}

/// `get_fork_messages`：会话历史里可作为 fork 点的用户消息。
fn fork_messages(agent: &Agent) -> Vec<Value> {
    let entries = agent
        .session
        .as_ref()
        .map(|s| s.get_entries())
        .unwrap_or_default();
    entries
        .iter()
        .filter(|entry| entry.get("type").and_then(Value::as_str) == Some("message"))
        .filter_map(|entry| {
            let message = entry.get("message")?;
            if message.get("role").and_then(Value::as_str) != Some("user") {
                return None;
            }
            let text = value_text(message);
            if text.is_empty() {
                return None;
            }
            Some(json!({
                "entryId": entry.get("id").and_then(Value::as_str).unwrap_or_default(),
                "text": text,
            }))
        })
        .collect()
}

/// 某个条目（消息型）的正文；找不到或非消息条目返回 `None`。
fn entry_text(agent: &Agent, entry_id: &str) -> Option<String> {
    agent
        .session
        .as_ref()?
        .get_entries()
        .into_iter()
        .find_map(|entry| {
            if entry.get("id").and_then(Value::as_str) != Some(entry_id) {
                return None;
            }
            Some(value_text(entry.get("message")?))
        })
}

/// 消息 JSON 里的文本块拼接（忽略思考与工具调用块）。
fn value_text(message: &Value) -> String {
    message
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

/// `get_commands`：可经 prompt 调用的扩展命令、提示词模板与技能。
fn commands_list(state: &State) -> Vec<Value> {
    let mut out: Vec<Value> = extensions::registered_commands()
        .into_iter()
        .map(|command| {
            json!({
                "name": command.name,
                "description": command.description,
                "source": "extension",
            })
        })
        .collect();

    for template in &state.templates {
        out.push(json!({
            "name": template.name,
            "description": template.description,
            "source": "prompt",
        }));
    }
    for skill in &state.skills {
        out.push(json!({
            "name": format!("skill:{}", skill.name),
            "description": skill.description,
            "source": "skill",
        }));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{core::agent_session::ToolSelection, test_support::AgentDirGuard};

    /// 一个不含会话、不联网的空 agent（设置面用线程本地临时目录隔离）。
    fn test_agent() -> Agent {
        let entry =
            model_resolver::find_model("deepseek", "deepseek-flash").expect("模型目录里有它");
        Agent::new(
            entry,
            "/tmp".to_string(),
            ToolSelection::tools(Vec::new()),
            Vec::new(),
            None,
            None,
            None,
            Vec::new(),
            None,
            None,
            true,
            Vec::new(),
        )
        .expect("构造 agent")
    }

    /// 忙碌期状态：只碰共享 Arc，不需要 Agent。
    fn busy_state() -> State {
        State {
            abort: Arc::new(AtomicBool::new(false)),
            steer: Arc::new(Mutex::new(VecDeque::new())),
            follow_ups: VecDeque::new(),
            skills: Vec::new(),
            templates: Vec::new(),
            busy: true,
            input_closed: false,
            quit: false,
            pending_internal: VecDeque::new(),
        }
    }

    /// 忙碌期只接得下队列类命令：排队命令回 queued，其余回「正在处理」，`abort` 立即置位。
    #[test]
    fn busy_mode_queues_input_and_rejects_other_commands() {
        let mut state = busy_state();

        let queued = handle_busy(
            &mut state,
            &json!({"id": "1", "type": "prompt", "message": "hi", "streamingBehavior": "steer"}),
        )
        .expect("有响应");
        assert_eq!(queued["success"], json!(true));
        assert_eq!(queued["data"]["disposition"], json!("queued"));
        assert_eq!(state.steer.lock().unwrap().len(), 1, "steer 进注入箱");

        let plain = handle_busy(
            &mut state,
            &json!({"id": "2", "type": "prompt", "message": "hi"}),
        )
        .expect("有响应");
        assert_eq!(plain["success"], json!(false));
        assert!(
            plain["error"]
                .as_str()
                .unwrap()
                .contains("streamingBehavior"),
            "缺 streamingBehavior 要指名道姓：{}",
            plain["error"]
        );

        let rejected =
            handle_busy(&mut state, &json!({"id": "3", "type": "get_state"})).expect("有响应");
        assert_eq!(rejected["success"], json!(false));
        assert!(rejected["error"].as_str().unwrap().contains("get_state"));

        let abort = handle_busy(&mut state, &json!({"id": "4", "type": "abort"})).expect("有响应");
        assert_eq!(abort["success"], json!(true));
        assert!(
            state.abort.load(Ordering::Relaxed),
            "abort 立即置位取消信号"
        );

        let cleared =
            handle_busy(&mut state, &json!({"id": "5", "type": "clear_queue"})).expect("有响应");
        assert_eq!(cleared["data"]["steering"], json!(["hi"]));
        assert!(state.steer.lock().unwrap().is_empty());
    }

    /// follow_up 在忙碌期进 settle 后队列，与 steer 不是同一条。
    #[test]
    fn follow_up_waits_for_the_next_turn() {
        let mut state = busy_state();
        let response = handle_busy(
            &mut state,
            &json!({"id": "1", "type": "follow_up", "message": "later"}),
        )
        .expect("有响应");
        assert_eq!(response["data"]["disposition"], json!("queued"));
        assert_eq!(state.follow_ups.len(), 1);
        assert!(state.steer.lock().unwrap().is_empty());
        assert_eq!(state.pending(), 1);
    }

    /// 附图作为 image 内容块追加在文本之后；没带 data 的图直接跳过。
    #[test]
    fn user_message_appends_images_after_text() {
        let message = user_message(
            "look",
            &json!({"images": [
                {"data": "AAA", "mimeType": "image/png"},
                {"mimeType": "image/jpeg"}
            ]}),
        );
        assert_eq!(message.role, "user");
        assert_eq!(message.content.len(), 2, "空图不算一条内容块");
        assert!(matches!(
            &message.content[1],
            ContentBlock::Image { data, mime_type } if data == "AAA" && mime_type == "image/png"
        ));
    }

    /// 只拼文本块：思考与工具调用不进 `text`。
    #[test]
    fn value_text_ignores_non_text_blocks() {
        let message = json!({"content": [
            {"type": "text", "text": "a"},
            {"type": "thinking", "thinking": "hmm"},
            {"type": "toolCall", "name": "bash"}
        ]});
        assert_eq!(value_text(&message), "a");
        assert_eq!(value_text(&json!({"content": []})), "");
    }

    /// 会话统计按角色计数并汇总 usage（无会话文件时退回当前上下文）。
    #[test]
    fn session_stats_count_roles_and_sum_usage() {
        let _guard = AgentDirGuard::temp();
        let mut agent = test_agent();
        let mut assistant = AgentMessage::user_text("");
        assistant.role = "assistant".to_string();
        assistant.usage = Some(Usage {
            input: 10,
            output: 5,
            cache_read: 2,
            cache_write: 1,
            total_tokens: 18,
            ..Default::default()
        });
        agent.messages = vec![
            AgentMessage::user_text("hi"),
            assistant,
            AgentMessage::user_text("tail"),
        ];

        let stats = session_stats(&agent);
        assert_eq!(stats["userMessages"], json!(2));
        assert_eq!(stats["assistantMessages"], json!(1));
        assert_eq!(stats["totalMessages"], json!(3));
        assert_eq!(stats["tokens"]["total"], json!(18));
        assert_eq!(stats["tokens"]["cacheRead"], json!(2));
        assert_eq!(stats["sessionId"], json!(EPHEMERAL_SESSION_ID));
    }
}
