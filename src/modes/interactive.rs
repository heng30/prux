//! 交互 TUI。状态驱动渲染（dirty 标志 + 自适应 poll）+ ratatui 组件化渲染。
//!
//! 模块划分：
//! - [`app`]      App 状态 + agent 事件处理
//! - [`handlers`] 键盘/事件分发
//! - [`render`]   渲染层：状态栏/消息区/输入框/底栏
//! - [`editor`]   多行输入编辑器
//! - [`theme`]    主题（主题 JSON → ratatui Style）

pub mod agent_actor;
pub mod app;
pub mod editor;
pub mod ext_settings;
pub mod handlers;
pub mod line_input;
pub mod oauth_flow;
pub mod overlay;
pub mod panel;
pub mod render;
pub mod session_selector;
pub mod settings_selector;
pub mod skills_panel;
pub mod system_theme;
pub mod theme;
pub mod worker;

use self::{
    agent_actor::WorkerHandle,
    app::{App, MsgLevel, NextAction, ReloadCtx, SysSpan, WorkingKind},
    handlers::{
        KeyAction, handle_event, take_staged_session_repair, take_staged_zombie_operations,
    },
    render::{
        messages::{CachedMsg, ImageLayout},
        render_frame, startup,
    },
    theme::{DEFAULT_THEME_NAME, Theme},
};
use crate::{
    APP_NAME,
    core::{
        self,
        agent_session::Agent,
        auth, crash_log, model_refresh, project_trust,
        prompt_templates::{self, PromptTemplate},
        provider::AgentMessage,
        settings_manager, tools_manager,
    },
    error::{Error, Result},
    modes::interactive::{
        self,
        panel::{PanelItem, PanelKind},
    },
    utils::{
        terminal_caps::{in_tmux, tmux_show_global_value},
        terminal_colors::prime_terminal_colors,
        terminal_image,
        time::now_ms,
        tokens,
        wheel::WheelScrollAccelerator,
    },
};
use crossterm::{
    event::EventStream,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures_util::StreamExt;
use ratatui::{Terminal, backend::CrosstermBackend};
use std::{
    cell::RefCell,
    collections::HashMap,
    io::{self, Write as _},
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

/// UI 任务队列元素：跨线程投递到主循环执行的闭包。
/// `run_in_event_loop` 的自由函数形态（任意线程一行调用），队列发送端存全局。
pub type UiTask = Box<dyn FnOnce(&mut App) + Send + 'static>;

/// 全局 UI 任务队列发送端（run_interactive 开头注册，退出清理）。
static UI_TASK_TX: Mutex<Option<UnboundedSender<UiTask>>> = Mutex::new(None);

/// 是否已收到 SIGTERM / SIGHUP 的退出请求。
///
/// 由信号处理器置位、主循环心跳分支消费（`false` = 无退出请求）。
static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

/// SIGTERM / SIGHUP 处理器：只置位 [`SHUTDOWN_REQUESTED`]。
///
/// 信号处理器里只允许 async-signal-safe 操作，因此不碰锁、不写盘、不打印；
/// 真正的退出（落盘 interrupted 记录、恢复终端）交给主循环。
#[cfg(unix)]
extern "C" fn request_shutdown(_signal: nix::libc::c_int) {
    SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
}

/// 安装退出信号处理器（SIGTERM / SIGHUP）。
///
/// 原先这两个信号无人处理：终端或 tmux pane 被关闭、遭外部 `kill` 时，进程按默认
/// 动作立即终止——会话文件停在半截、不补 interrupted 记录、也没有崩溃日志，
/// 看起来就是「跑到一半无端退出」。捕获后转为正常退出流程，至少留下可诊断痕迹。
#[cfg(unix)]
fn install_shutdown_signal_handlers() {
    use nix::sys::signal::{SigHandler, Signal, signal};
    unsafe {
        _ = signal(Signal::SIGTERM, SigHandler::Handler(request_shutdown));
        _ = signal(Signal::SIGHUP, SigHandler::Handler(request_shutdown));
    }
}

/// Windows 无 SIGHUP / SIGTERM：空实现（关窗口即强制结束进程）。
#[cfg(not(unix))]
fn install_shutdown_signal_handlers() {}

/// `run_in_event_loop` 失败原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventLoopError {
    /// TUI 事件循环已退出 / 尚未启动（队列关闭或未注册）。
    Closed,
}

impl std::fmt::Display for EventLoopError {
    /// 输出固定文案 `"TUI event loop closed"`（不区分具体变体）。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EventLoopError::Closed => write!(f, "TUI event loop closed"),
        }
    }
}

impl std::error::Error for EventLoopError {}

/// 注册全局 UI 任务发送端（主循环启动时调用）。
pub fn set_event_loop_tx(tx: UnboundedSender<UiTask>) {
    *UI_TASK_TX.lock().unwrap() = Some(tx);
}

/// 清理全局 UI 任务发送端（主循环退出时调用）。
pub fn clear_event_loop_tx() {
    *UI_TASK_TX.lock().unwrap() = None;
}

/// 把 `func` 投递到 TUI 主循环任务队列；主循环空闲间隙（select 分支）执行。
/// 适合任意后台线程/任务把「修改 UI 状态」的意图直接送达 `&mut App`
pub fn run_in_event_loop(
    func: impl FnOnce(&mut App) + Send + 'static,
) -> std::result::Result<(), EventLoopError> {
    let Some(tx) = UI_TASK_TX.lock().unwrap().clone() else {
        return Err(EventLoopError::Closed);
    };
    tx.send(Box::new(func)).map_err(|_| EventLoopError::Closed)
}

/// 后台补高亮完成回调：渲染结果 + 提交指纹（提交前与当前 App 状态比对）；
/// snap_id 标识产生结果的快照，切换会话/重新快照后旧结果直接丢弃
struct HighlightDone {
    /// 产生结果的快照标识；与 App 当前快照不一致时丢弃结果。
    snap_id: u64,
    /// 快照时的消息条数，提交前校验列表未变化。
    msg_len: usize,
    /// 渲染使用的终端列宽。
    width: usize,
    /// 快照时是否展开全部块（提交前校验）。
    expand_all: bool,
    /// 快照时是否显示思考内容（提交前校验）。
    show_thinking: bool,
    /// 快照时的内联图片渲染参数（提交前校验；开关或单元格尺寸变了就丢弃结果）。
    images: ImageLayout,
    /// 渲染结果，键为（消息时间戳 ts，序号 seq）。
    results: HashMap<(u64, usize), CachedMsg>,
}

/// 交互模式入口
#[allow(clippy::too_many_arguments)]
pub async fn run_interactive(
    agent: Agent,
    initial_messages: Vec<AgentMessage>,
    prompt_templates: Vec<PromptTemplate>,
    theme_override: Option<&str>,
    no_themes: bool,
    reload_ctx: ReloadCtx,
    verbose: bool,
) -> Result<()> {
    // 终端配色查询（OSC 10/11/4）：必须在 `setup_terminal` 开启 raw 模式**之前**做——
    // 查询自身要临时开关 raw 模式，且要在 TUI 接管输入前把回复读掉。
    // `system` 主题（默认）据此推导整套色板；终端不上报时回落到内置 dark/light。
    prime_terminal_colors();

    // 内联图片：只在开关打开时探测终端图形能力。查询会读写 stdin，
    // 必须早于 crossterm 的事件流（否则回复会被当成按键吃掉），所以在这里做。
    terminal_image::prime_image_picker(settings_manager::read_settings_show_images());

    let mut terminal = setup_terminal()?;
    install_shutdown_signal_handlers();

    // UI 任务队列：任何后台线程经 run_in_event_loop 投闭包 → 主循环这里执行。
    // 单一队列兼管 worker 回执 / 文件扫描 / 工具状态 / 高亮补全等一切后台→UI 更新。
    let (ui_task_tx, mut ui_task_rx) = unbounded_channel::<UiTask>();
    set_event_loop_tx(ui_task_tx);

    // 预热 tiktoken 词表（首次编码要解析 ~2MB 词表）：后台线程先跑，避免首次流式/压缩统计在事件循环上卡顿
    let model_id = agent.model.model_id.clone();
    std::thread::spawn(move || tokens::warmup(Some(&model_id)));

    let shared = init_app(
        &agent,
        initial_messages,
        prompt_templates,
        theme_override,
        no_themes,
        reload_ctx,
        verbose,
    );

    // agent actor：worker 独占 agent，UI 经命令通道发送指令、经闭包队列收状态更新
    let (worker_cmd_tx, worker_cmd_rx) = agent_actor::channels();
    let worker = WorkerHandle::new(worker_cmd_tx);
    shared.borrow_mut().worker = worker.clone();
    let worker_task = tokio::spawn(worker::run_worker(
        Arc::new(tokio::sync::Mutex::new(agent)),
        worker_cmd_rx,
    ));
    let mut event_stream = EventStream::new();

    // 启动时检测 tmux 键盘协议配置（extended-keys / csi-u），不符合则提示用户改 tmux.conf
    spawn_tmux_keyboard_check();

    loop {
        update_terminal_title(&shared);
        render_tick(&shared, &mut terminal)?; // 渲染（状态驱动：dirty 或忙碌时重绘）

        // 恢复启动、`/resume` 切换会话：首帧 Fast 渲染（无语法高亮）完成后，
        // 启动后台补全线程，用完整高亮重渲染全部历史并替换消息级缓存；
        // 结果经闭包回主循环，提交前校验快照 ID 与指纹（消息数/宽度/参数），
        // 期间变化则丢弃并在下一帧重新快照。
        maybe_start_backlog_highlight(&shared);

        // 单 select 循环：UI 任务队列 / 键盘 / 定时器。
        // UI 不持有回合 future——回合在 worker 内执行，状态经闭包回流。
        tokio::select! {
            task = ui_task_rx.recv() => {
                if let Some(task) = task {
                    let mut st = shared.borrow_mut();
                    task(&mut st);
                    st.dirty = true;
                }
            }
            // 用户输入分支：EventStream 是 crossterm 的终端读流（非手动发送），
            // 内部单例 InternalEventReader 直接 poll tty fd，用户按键/鼠标/粘贴/窗口
            // resize 时解析为 Event 并唤醒本分支；setup_terminal 开启的 raw mode、
            // bracketed paste、mouse capture、kitty 键盘协议保证字节可解析。
            ev = event_stream.next() => {
                let Some(Ok(ev)) = ev else { continue };
                if process_key_action(&shared, handle_event(&shared, ev)) {
                    break;
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(80)) => {
                // SIGTERM / SIGHUP：终端或 tmux pane 被关闭、遭外部 kill。
                // 走正常退出流程（补 interrupted 记录 + 恢复终端），不留半截会话。
                if SHUTDOWN_REQUESTED.swap(false, Ordering::SeqCst) {
                    break;
                }

                // 忙碌动画 tick：无新事件时也保持重绘，spinner 持续旋转；
                // 顺带过期清理状态提示（忙碌时也需要 2s 提示准时消失），
                // 以及每秒刷新消息区重试倒计时（upsert_sys_msg 自带 dirty）。
                let mut st = shared.borrow_mut();
                if st.expire_status() || st.auto_scroll_tick() || overlay::auto_scroll_tick(&mut st)
                {
                    st.dirty = true;
                }

                // 后台活儿（后台代理 / 工作流运行）在跑 → dock 里的 spinner 与计数需要动画
                if core::extensions::wants_redraw() {
                    st.dirty = true;
                }

                // /resume 选择器渐进加载：每帧补齐一批
                if st.session_selector.is_loading()
                    && st
                        .session_selector
                        .drain_pending(session_selector::DRAIN_BATCH)
                {
                    st.dirty = true;
                }

                st.retry_ui_tick();
            }
        }

        // OAuth 登录回调/手动输入轮询
        poll_oauth(&shared).await;

        // 扩展 UI 请求：选择面板 / 通知（Select 结果经 on_ui_choice 回传扩展）
        maybe_handle_extension_ui(&shared);

        // 会话修复请求（启动路径 stage 的损坏会话）：首帧弹确认面板
        maybe_handle_session_repair(&shared);

        // “未完成 operation”提示请求（启动路径打开的会话存在崩溃残留）：首帧弹选择面板
        maybe_handle_zombie_operations(&shared);

        // settings.json 损坏的覆写确认：core 推迟写入时弹确认面板（Overwrite / Cancel）
        maybe_handle_settings_overwrite(&shared);

        // 取出待执行动作并作为命令下发给 worker（None 无动作）
        let action = shared.borrow_mut().take_next_action();
        start_next_action(&shared, action);
    }

    // 恢复终端状态（退出流程）
    clear_event_loop_tx();
    // 退出前清理：当前会话为空（无用户消息）则删除
    shared.borrow_mut().delete_current_session_if_empty();

    // 通知 worker 收尾（丢弃进行中的回合并为未闭合 operation 补 interrupted），
    // 等待其退出，避免进程结束时磁盘残留僵尸 operation（Esc/Ctrl+C 后 /quit 的复现路径）。
    worker.send(agent_actor::AgentCommand::Shutdown);
    _ = tokio::time::timeout(Duration::from_secs(3), worker_task).await;

    teardown_terminal(&mut terminal);
    Ok(())
}

/// 处理单个键盘事件对应的动作；返回 true 表示退出
fn process_key_action(shared: &Rc<RefCell<App>>, action: KeyAction) -> bool {
    match action {
        KeyAction::Quit => true,
        KeyAction::CancelPrompt => {
            // 协作式中止当前回合：worker 经 oneshot 触达回合内 agent；
            // UI 本地清流式缓冲并复位 busy（agent_end 事件会补齐最终消息）。
            shared.borrow_mut().worker.abort();
            handle_cancel_prompt(shared);
            false
        }
        KeyAction::Continue => false,
    }
}

/// settle 后取下一批待发送消息（steer 先于 follow-up，同类按 mode
/// all=整批 / one-at-a-time=一条；同类内部 FIFO）。steer 主要在 run_loop 内中途
/// 注入（inject_runtime_steers），此处负责 one-at-a-time 下当前轮未消费完的
/// 剩余 steer，以及所有 follow-up。
fn drain_next_batch(st: &mut App) -> Option<Vec<AgentMessage>> {
    let steer_mode = settings_manager::read_settings_steering_mode();
    let follow_up_mode = settings_manager::read_settings_follow_up_mode();

    // 1. steer（运行中注入箱）优先：当前轮未消费完的剩余 steer 续跑
    let steer_batch: Vec<AgentMessage> = {
        let mut inbox = st.runtime_steer_inbox.lock().unwrap();
        if inbox.is_empty() {
            Vec::new()
        } else if steer_mode == "all" {
            inbox.drain(..).collect()
        } else {
            inbox.pop_front().into_iter().collect()
        }
    };
    if !steer_batch.is_empty() {
        return Some(steer_batch);
    }

    // 2. follow-up（本地队列，settle 后发送）
    let mut fups = Vec::new();
    while let Some(fup) = st.runtime_followup_inbox.pop_front() {
        fups.push(fup);
        if follow_up_mode != "all" {
            break;
        }
    }

    if !fups.is_empty() {
        return Some(
            fups.into_iter()
                .map(|t| AgentMessage::user_text(&t))
                .collect(),
        );
    }

    None
}

/// 进入备用屏幕 + raw mode + 键盘增强协议（kitty protocol）。
/// REPORT_ALL_KEYS_AS_ESCAPE_CODES 让所有按键以 CSI u 上报完整修饰符（否则 Ctrl+Shift+C
/// 会被终端降级为 \x03，与普通 Ctrl+C 无法区分），终端不支持时静默降级（忽略错误）。
fn update_terminal_title(shared: &Rc<RefCell<App>>) {
    let basename = {
        let st = shared.borrow();
        Path::new(&st.cwd)
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default()
    };

    // OSC 0 (icon+title) / OSC 2 (title)
    _ = write!(
        std::io::stdout(),
        "\x1b]0;{APP_NAME} - {basename}\x07\x1b]2;{APP_NAME} - {basename}\x07"
    );
    _ = std::io::stdout().flush();
}

/// 初始化终端：开 raw mode、进备用屏、开 bracketed paste，并请求终端透传鼠标与扩展键
/// （tmux 内走 xterm modifyOtherKeys，否则用 crossterm 的 kitty 增强键协议）。
/// 任一步失败返回 `Err`（调用方负责提示并退出）；成功返回可供 ratatui 绘制的终端句柄。
fn setup_terminal() -> Result<Terminal<CrosstermBackend<io::Stdout>>> {
    let mut stdout = io::stdout();
    terminal::enable_raw_mode()
        .map_err(|e| Error::msg(format!("failed to enable raw mode: {}", e)))?;
    crossterm::execute!(
        stdout,
        EnterAlternateScreen,
        crossterm::event::EnableBracketedPaste,
    )
    .map_err(|e| Error::msg(format!("failed to initialize terminal: {}", e)))?;

    // tmux 内：全量鼠标模式 + xterm modifyOtherKeys 扩展键请求，一次写入。
    // 鼠标：prux 是全屏 mouse owner（拖选/边缘滚动/滚动条/中键复制粘贴），必须让 tmux
    // 把鼠标事件全透传（1003 置位 mouse_any_flag；否则 MouseDrag1Pane 进 copy-mode -M、
    // MouseDown2Pane 执行 paste-buffer -p，prux 收不到拖选和中键——tmux 里“选中后
    // Ctrl+Shift+C / 中键复制无效”的根因之一）。1004 focus 事件对齐 pi。
    // 键盘：kitty push（CSI > flags u）tmux 3.6a/3.7c 都不识别，pane_key_mode 停在 VT10x，
    // Shift 系修饰键全丢；xterm modifyOtherKeys 请求（CSI > 4;2m）被识别后 pane_key_mode
    // 变 Ext 2，配合 extended-keys-format csi-u 才把修饰键以 CSI-u 编码给 prux
    // （ditto pi：tmux 内请求 extended key reporting 而非 kitty）。
    // 非 tmux 用 crossterm：鼠标 1000+1002+1006（终端无需 1003 也上报 Drag），
    // 键盘 push kitty protocol（DISAMBIGUATE | REPORT_ALL_KEYS_AS_ESCAPE_CODES）。
    if in_tmux() {
        write!(
            stdout,
            "\x1b[>4;2m\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1004h\x1b[?1006h"
        )
        .and_then(|_| stdout.flush())
        .map_err(|e| Error::msg(format!("failed to setup tmux terminal: {}", e)))?;
    } else {
        crossterm::execute!(
            stdout,
            crossterm::event::EnableMouseCapture,
            crossterm::event::PushKeyboardEnhancementFlags(
                crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | crossterm::event::KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES
            )
        )
        .map_err(|e| Error::msg(format!("failed to initialize terminal: {}", e)))?;
    }

    let backend = CrosstermBackend::new(stdout);
    Terminal::new(backend)
        .map_err(|e| Error::msg(format!("failed to initialize terminal backend: {}", e)))
}

/// 初始化 TUI 共享状态：同步 agent 会话快照 + 主题/设置/信任询问等启动逻辑。
fn init_app(
    agent: &Agent,
    initial_messages: Vec<AgentMessage>,
    prompt_templates: Vec<PromptTemplate>,
    theme_override: Option<&str>,
    no_themes: bool,
    reload_ctx: ReloadCtx,
    verbose: bool,
) -> Rc<RefCell<App>> {
    // TUI 共享状态只在单任务事件循环使用，Arc 仅为对外接口统一，不会跨线程 send
    #[allow(clippy::arc_with_non_send_sync)]
    let shared = Rc::new(RefCell::new(App::new()));

    {
        let mut st = shared.borrow_mut();
        let agent_cwd = agent.cwd.clone();

        st.theme = Theme::load(theme_override.unwrap_or(DEFAULT_THEME_NAME));
        st.messages = agent.messages.clone();
        st.runtime_steer_inbox = agent.runtime_steer_inbox.clone();
        st.current_model = Some(agent.model.model_id.clone());
        st.current_provider = Some(agent.model.provider.clone());

        // 缓存持久化的默认模型/思考级别（仅 /model、/thinking 面板 Ctrl+S 会更新），
        // 供面板渲染 `· default` 标记；普通切换不写盘也不改这些字段。
        let settings = settings_manager::read_settings();
        st.default_provider = settings.default_provider;
        st.default_model = settings.default_model;
        st.default_thinking_level = settings.default_thinking_level;

        // quietStartup：`true` 不显示启动横幅；`"header"`/`false` 决定是否输出资源清单。
        let quiet_startup = settings_manager::read_settings_quiet_startup();
        st.banner_hidden = !(verbose || quiet_startup.shows_banner());

        // 启动资源清单（模型/会话/工具/上下文/技能/模板/主题/扩展）
        if verbose || quiet_startup.shows_details() {
            let facts = startup::collect(agent, &prompt_templates);
            st.push_msg_rich(startup::render(&facts), MsgLevel::Info);
        }

        // 未配置任何模型（无 CLI/env/settings）落到内置默认：启动即告知，避免误以为配置失效。
        // 兜底 provider 若也未配置凭据，兜底模型实际不可用：不声称 "using ..."，footer 不显示模型名。
        if agent.model_is_fallback {
            let fallback_available =
                auth::list_configured_providers().contains(&agent.model.provider);
            st.no_model_available = !fallback_available;
            if fallback_available {
                st.push_msg(
                    format!(
                        "No model configured — using {} ({}); open /model to choose",
                        agent.model.model_id, agent.model.provider
                    ),
                    MsgLevel::Warning,
                );
            } else {
                st.push_msg(
                    "No model configured; open /model to choose".to_string(),
                    MsgLevel::Warning,
                );
            }
        }

        st.model_reasoning = agent.model.reasoning;
        st.thinking_level = agent.thinking_level.clone();
        st.thinking_levels = agent
            .get_available_thinking_levels()
            .iter()
            .map(|s| s.to_string())
            .collect();
        st.context_window = agent.model.context_window;
        st.current_session_path = agent
            .session
            .as_ref()
            .and_then(|s| s.get_session_file())
            .map(|p| p.to_string_lossy().to_string());

        st.theme.latex = settings_manager::read_settings_latex();
        st.theme.mermaid = settings_manager::read_settings_mermaid();
        st.theme.syntax_highlight = settings_manager::read_settings_syntax_highlight();

        // 恢复历史会话启动（-c/--resume/--session/--fork/--session-id 均会载入历史消息）：
        // 首帧用 Fast 渲染（跳过 syntect 语法高亮）秒开，后台补全线程随后用完整高亮
        // 重渲染历史并替换消息级缓存。全新会话（无历史消息）或用户禁用语法高亮时
        // 不在此列，直接按当前开关同步渲染。
        // 历史几乎总有滚动条 → 首帧直接按滚动条宽度（width-1）渲染，避免双渲染。
        if !st.messages.is_empty() && st.theme.syntax_highlight {
            st.last_has_scrollbar = true;
            st.highlight_backlog = true;
            st.theme.syntax_highlight = false;
        }

        st.prompt_templates = prompt_templates;

        // frontmatter 不合法的 prompt 模板：启动时提示而非静默忽略
        for w in prompt_templates::take_load_warnings() {
            st.push_msg(w, MsgLevel::Warning);
        }

        // 上次进程崩溃过（panic 钩子写进 crashes.json）：提示一次，/bug 会自动带上崩溃详情
        if let Some(crash) = crash_log::take_unnotified_crash() {
            st.push_msg(
                format!(
                    "{} crashed on {} ({}). Run /bug to report it; the crash details are attached automatically.",
                    APP_NAME, crash.timestamp, crash.message
                ),
                MsgLevel::Warning,
            );
        }

        st.skills = agent.skills.clone();
        st.model_cycle = agent.model_cycle.clone();
        st.reload_ctx = reload_ctx;
        st.initial_queue = initial_messages.into_iter().collect();
        st.no_themes = no_themes;
        st.cwd = agent_cwd.clone();
        st.refresh_context_percent();

        st.set_show_thinking(!settings_manager::read_settings_hide_thinking());
        st.show_images = settings_manager::read_settings_show_images();
        st.cache_images_fp = (st.show_images, terminal_image::image_font_size());
        st.autocomplete_max_visible = settings_manager::read_settings_autocomplete_max_visible();
        st.wheel_accel = WheelScrollAccelerator::for_terminal(
            settings_manager::read_settings_fullscreen_wheel_scroll_lines(),
        );
        st.history_max_entries = settings_manager::read_settings_history_max_entries();
        st.editor.history = core::prompt_history::load(&st.cwd, st.history_max_entries)
            .into_iter()
            .map(|e| e.text)
            .collect();

        // 未配置任何 provider 凭据（auth.json 或 models.json apiKey）：
        // 消息流给出 /login 指引（不阻塞启动）；配了 Anthropic federation 环境变量但本
        // 版本不支持 token 交换时追加说明，避免「No API keys configured」让人找不到原因。
        if auth::list_configured_providers().is_empty() {
            let note = auth::federation_note("anthropic")
                .map(|n| format!(" {n}"))
                .unwrap_or_default();
            st.push_msg(
                format!("No API keys configured. Run /login to store a provider API key.{note}"),
                MsgLevel::Warning,
            );
        } else if let Some(w) = startup_model_warning(
            &agent.model.provider,
            &agent.model.model_id,
            agent.api_key_override.is_some(),
        ) {
            // 已配置部分凭据但当前 provider/model 组合不在 /model 面板列表
            // （provider 未登录、模型不在目录、或目录里没有该 provider 的模型）：
            // 启动即提示登录或切换模型，避免首轮请求无凭据/无模型时静默失败
            st.push_msg(w, MsgLevel::Warning);
        }

        // 无效 settings 文件：TUI 中显示警告而非静默忽略
        if let Some(e) = settings_manager::settings_load_error() {
            st.push_msg(e, MsgLevel::Warning);
        }

        // 内置 find/grep 已启用、但后端程序（fd/rg）缺失：给出下载地址 / `/download` 指引。
        // 只提示，不后台自动下载（安装由 downloader 扩展的 /download 触发）。
        for hint in tools_manager::missing_tool_hints(&agent.get_active_tools()) {
            st.push_msg(hint, MsgLevel::Warning);
        }

        // 扩展声明问题：非法命令名（空名 / 含 `/`、空白）与重名（同名工具 /
        // 斜杠命令 / CLI flag，先注册者生效、后注册者被丢弃）→ 逐条告警
        for warning in core::extensions::registration_issues() {
            st.push_msg(warning, MsgLevel::Warning);
        }

        // 启动时清理 models-store.json 中无凭据的残留 provider 条目
        // （models-store.json 只保存已 /login 的 provider；
        // 早期版本的启动拉取可能遗留未登录条目，在这里清扫保持洁净）
        model_refresh::cleanup_stale_entries();

        // 项目信任 ask：仅在存在需要信任的项目资源时询问
        maybe_ask_project_trust(&mut st, &agent_cwd);
    }
    shared
}

/// 计算启动时的模型可用性提示（None = 无需额外提示）。
/// 无任何凭据时返回 None（init_app 已有 “No API keys configured” 提示覆盖）；
/// 其余场景与 /model 面板同口径（provider 在 list_configured_providers 且
/// 模型在 list_models(provider) 目录中）：组合不在面板列表即提示登录或切换模型。
fn startup_model_warning(
    provider: &str,
    model_id: &str,
    has_api_key_override: bool,
) -> Option<String> {
    if auth::list_configured_providers().is_empty() || has_api_key_override {
        return None;
    }
    if model_in_panel_list(provider, model_id) {
        return None;
    }
    Some(format!(
        "Model {}/{} is not available in the /model panel; run /login to add credentials or use /model to switch",
        provider, model_id
    ))
}

/// 当前 provider+model 组合是否能在 /model 面板列表中找到。
/// 与 handlers::auth::build_model_panel_items 同口径：provider 须在
/// list_configured_providers（auth.json / models.json apiKey / 环境变量），
/// 模型须在 list_models(provider) 目录中。
fn model_in_panel_list(provider: &str, model_id: &str) -> bool {
    if !auth::list_configured_providers()
        .iter()
        .any(|p| p == provider)
    {
        return false;
    }
    core::model_resolver::list_models(provider)
        .iter()
        .any(|(id, _)| id == model_id)
}

/// 项目信任 ask：仅在存在需要信任的项目资源、尚无显式决策、且默认策略为 Ask 时
/// 弹出选择面板（显式决策/--approve/--no-approve 均不再询问）。
/// 不弹面板而项目未被信任时，给出一行未信任警告。
fn maybe_ask_project_trust(st: &mut App, agent_cwd: &str) {
    let cwd_path = PathBuf::from(&agent_cwd);
    let agent_dir = settings_manager::agent_dir();
    let requires_trust = project_trust::has_trust_requiring_project_resources(&cwd_path);

    let prompt = st.reload_ctx.trust_override.is_none()
        && requires_trust
        && project_trust::trust_decision(&cwd_path, &agent_dir).is_none()
        && project_trust::default_project_trust(&agent_dir) == project_trust::ProjectTrust::Ask;

    if prompt {
        st.open_project_trust_panel();
        return;
    }

    // 无面板可弹（已保存 false / default=never / --no-approve）：未信任时警告
    st.warn_if_project_untrusted();
}

/// 启动时检测 tmux 键盘协议配置，缺哪层补哪层提示：
/// 1. tmux 版本缺陷：3.6/3.6a 与 foot/xterm 的组合即使配置全对也丢修饰键（tmux#4880），
///    修复在 3.7（重构 extended keys 支持）；
/// 2. 客户端终端 extkeys 特性（terminal-features）：缺失时 tmux 不会向外部终端请求
///    修饰键，Shift+Enter / Ctrl+Shift+C 等 modifier 在外部终端（foot/kitty）源头就丢失；
/// 3. `extended-keys on` + `extended-keys-format csi-u`：tmux 才把 CSI-u 完整修饰键
///    序列转发给应用（否则被重编码/吞掉）。
///
/// 只读探测并提示，不修改用户 tmux 配置；非 tmux/查询失败静默。
fn spawn_tmux_keyboard_check() {
    if !in_tmux() {
        return;
    }

    std::thread::spawn(move || {
        // 配置缺失：extkeys 特性（tmux 不向外部终端请求修饰键，Shift 在源头丢失）
        // if !tmux_client_supports_extkeys() {
        //     let w = format!(
        //         "tmux 未向外部终端请求扩展键（client_termfeatures 缺 extkeys）：\n\
        //          Shift 系修饰键（Shift+Enter / Ctrl+Shift+C 等）会丢失。\n\
        //          请在 tmux 配置添加（终端名按实际调整，如 foot/kitty/ghostty/wezterm）：\n\
        //          `set -ag terminal-features 'foot*:extkeys'`\n\
        //          然后重启 tmux。"
        //     );
        //     _ = run_in_event_loop(move |ui| ui.push_msg(w, MsgLevel::Warning));
        // }

        let Some(extended_keys) = tmux_show_global_value("extended-keys") else {
            return;
        };
        let warning = if extended_keys != "on" && extended_keys != "always" {
            Some(format!(
                "tmux extended-keys is {}; modified keys (Ctrl+Shift+..., Shift+Enter) may \
                 not be distinguishable. Add `set -g extended-keys on` to ~/.tmux.conf and \
                 restart tmux.",
                extended_keys
            ))
        } else if let Some(format) = tmux_show_global_value("extended-keys-format") {
            if format == "xterm" {
                Some(format!(
                    "tmux extended-keys-format is xterm; {APP_NAME} works best with csi-u. \
                     Add `set -g extended-keys-format csi-u` to ~/.tmux.conf and restart tmux."
                ))
            } else {
                None
            }
        } else {
            None
        };

        if let Some(w) = warning {
            _ = run_in_event_loop(move |ui| ui.push_msg(w, MsgLevel::Warning));
        }
    });
}

/// 恢复启动和 `/resume` 切换会话：首帧 Fast 渲染（无语法高亮）完成后，
/// 启动后台补全线程，用完整高亮重渲染全部历史并替换消息级缓存；
/// 结果经闭包投递回主循环，提交前校验快照 ID 与指纹（消息数/宽度/参数），
/// 期间变化则丢弃并在下一帧重新快照。
fn maybe_start_backlog_highlight(shared: &Rc<RefCell<App>>) {
    let hl_snapshot = {
        let mut st = shared.borrow_mut();
        if st.highlight_backlog && !st.highlight_started {
            st.highlight_started = true;
            st.highlight_snap_id += 1;
            Some((st.highlight_snap_id, st.build_highlight_snapshot()))
        } else {
            None
        }
    };

    if let Some((snap_id, snap)) = hl_snapshot {
        let done = HighlightDone {
            msg_len: snap.msgs.len(),
            width: snap.width,
            expand_all: snap.expand_all,
            show_thinking: snap.show_thinking,
            images: snap.images,
            snap_id,
            results: HashMap::new(),
        };

        std::thread::spawn(move || {
            let results = interactive::render::messages::render_full_history(&snap);
            _ = run_in_event_loop(move |ui| {
                on_highlight_done(
                    ui,
                    HighlightDone {
                        msg_len: done.msg_len,
                        width: done.width,
                        expand_all: done.expand_all,
                        show_thinking: done.show_thinking,
                        images: done.images,
                        snap_id,
                        results,
                    },
                );
            });
        });
    }
}

/// 渲染步骤（状态驱动：dirty 或忙碌时重绘）。
/// 外部编辑器返回时 terminal.clear() 重置 ratatui 的 previous buffer，
/// 下一次 draw 全量重绘（编辑器残留内容会让 diff 增量绘制错乱）。
fn render_tick(
    shared: &Rc<RefCell<App>>,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
) -> Result<()> {
    let mut st = shared.borrow_mut();
    // 主题变化后同步给扩展与 HTML 导出（指纹未变时空跑）
    st.theme.publish_view();

    if st.full_redraw {
        terminal
            .clear()
            .map_err(|e| Error::msg(format!("failed to clear terminal: {}", e)))?;
        st.full_redraw = false;
        st.dirty = true;
    }
    if st.should_repaint() {
        terminal
            .draw(|f| render_frame(f, &mut st))
            .map_err(|e| Error::msg(format!("failed to render: {}", e)))?;
        st.dirty = false;
    }
    Ok(())
}

/// 处理后台补高亮结果：只接受最新快照的结果（切换会话/重新快照后，旧线程结果
/// 直接丢弃、不触碰任何状态，避免误清 highlight_started 或过早恢复高亮打断
/// /resume 后的 Fast 渲染）；再校验指纹（消息数/宽度/参数），期间变化则丢弃结果，并在下一帧重新快照。
fn on_highlight_done(st: &mut App, done: HighlightDone) {
    if done.snap_id != st.highlight_snap_id {
        return;
    }

    // 再校验指纹（消息数/宽度/参数）
    let ok = st.messages.len() == done.msg_len
        && st.cache_width == done.width
        && st.cache_expand_all == done.expand_all
        && st.cache_show_thinking == done.show_thinking
        && st.image_layout().unwrap_or_else(ImageLayout::disabled) == done.images;

    if ok {
        // 补高亮完成：恢复语法高亮开关，之后所有新消息/流式段/缓存重建都用完整高亮渲染。
        st.theme.syntax_highlight = true;
        st.msg_cache.extend(done.results);
        st.highlight_backlog = false;
        st.dirty = true;
    } else {
        // 渲染期间消息/宽度/参数变化：丢弃结果，下帧重新快照再试
        st.highlight_started = false;
    }
}

/// 取消当前 prompt（Esc/Ctrl+C）：UI 本地清理 + 复位 busy。
/// agent 侧回合经 abort oneshot 被硬中止（prompt future 被 drop），不再有 agent_end
/// 事件带回最终消息——中断提示由本函数补写的 stop_reason=aborted 消息渲染呈现；
/// 这里只清流式缓冲、补中断提示并复位忙碌状态（不再直接写 agent）。
fn handle_cancel_prompt(shared: &Rc<RefCell<App>>) {
    let mut st = shared.borrow_mut();
    apply_hard_abort_state(&mut st);

    // 中断（Esc/Ctrl+C）时清空全部排队消息并取回编辑器，绝不续跑下一条
    let steers: Vec<String> = st
        .runtime_steer_inbox
        .lock()
        .unwrap()
        .drain(..)
        .map(|m| m.text())
        .collect();
    let follow_ups: Vec<String> = st.runtime_followup_inbox.drain(..).collect();

    if !steers.is_empty() || !follow_ups.is_empty() {
        let mut texts = steers;
        texts.extend(follow_ups);
        let queued = texts.join("\n\n");
        let current = st.editor.text();
        let combined = if current.trim().is_empty() {
            queued
        } else {
            format!("{}\n\n{}", queued, current)
        };
        st.editor.clear();
        st.editor.insert_text(&combined);
    }
}

/// 硬中止（Esc/Ctrl+C 或扩展 `AbortRun` 请求）的公共 UI 状态收尾：
/// 丢弃流式缓冲、补写 aborted 标记、复位 busy/状态栏。
/// 调用方自行处理排队消息（Esc 取回编辑器，扩展续跑保留队列）。
fn apply_hard_abort_state(st: &mut App) {
    // 向扩展分发 agent_end，复位 footer 计时等状态。
    // `reason: "aborted"` 区分「用户/扩展主动中断」与「回合自然结束」：
    core::extensions::dispatch_agent_event(&serde_json::json!({
        "type": "agent_end",
        "reason": "aborted",
    }));

    st.clear_streaming();
    st.tool_started_at.clear();
    st.last_elapsed_rebuild = None;

    // 硬中止：worker 经 oneshot 直接 drop prompt future，agent 不会再吐
    // agent_end / stop_reason=aborted 消息——消息区的中断提示需要在这里补写：
    // 渲染层对 stop_reason=aborted 的 assistant 消息追加红色 “Operation aborted”。
    let mut aborted = AgentMessage::user_text("");
    aborted.role = "assistant".to_string();
    aborted.stop_reason = Some("aborted".to_string());
    aborted.error_message = Some("Operation aborted".to_string());
    aborted.timestamp = now_ms();
    st.messages.push(aborted);

    // 流式锚点失效：本回合 assistant 已被替换成固定 aborted 消息，不再渲染流式尾巴
    st.stream_anchor_index = None;

    st.busy = false;
    // 中断：worker abort 会 drop prompt future，auto_retry_end 事件不会来，重试提示原地覆盖为已取消并停止每秒刷新
    st.cancel_retry_ui();
    // 中断提示已由消息区渲染 stop_reason=aborted 的红色提示呈现，status 不重复显示
    st.clear_status();
    st.dirty = true;
}

/// OAuth 登录回调/手动输入轮询
/// 持锁 await：单任务事件循环内 poll 顺序执行，锁不会跨 task 竞争。
#[allow(clippy::await_holding_refcell_ref)]
async fn poll_oauth(shared: &Rc<RefCell<App>>) {
    let oauth_done = oauth_flow::poll(&mut shared.borrow_mut()).await;
    if oauth_done {
        shared.borrow_mut().dirty = true;
    }
}

/// 扩展 UI 请求处理：
/// - Select → 打开通用自定义面板（Enter/Esc 结果经 on_ui_choice 按请求 id 回传扩展）；
/// - Notify → 聊天区系统消息。
fn maybe_handle_extension_ui(shared: &Rc<RefCell<App>>) {
    let Some(req) = core::extensions::take_pending_ui() else {
        return;
    };

    let mut st = shared.borrow_mut();
    match req {
        core::extensions::ExtensionUiRequest::Select { id, title, options } => {
            let items: Vec<PanelItem> = options
                .into_iter()
                .map(|o| match o.fg {
                    Some(fg) => PanelItem::styled(o.text.clone(), o.text, fg),
                    None => PanelItem::new(o.text.clone(), o.text),
                })
                .collect();
            st.panel.close();
            st.panel.open(PanelKind::Custom(id), title, items);
        }
        core::extensions::ExtensionUiRequest::Notify { text, level } => {
            let level = match level {
                core::extensions::UiNotifyLevel::Info => MsgLevel::Info,
                core::extensions::UiNotifyLevel::Success => MsgLevel::Success,
                core::extensions::UiNotifyLevel::Warning => MsgLevel::Warning,
                core::extensions::UiNotifyLevel::Error => MsgLevel::Error,
            };
            st.push_msg(text, level);
        }
        core::extensions::ExtensionUiRequest::NotifyRich { spans, level } => {
            let level = match level {
                core::extensions::UiNotifyLevel::Info => MsgLevel::Info,
                core::extensions::UiNotifyLevel::Success => MsgLevel::Success,
                core::extensions::UiNotifyLevel::Warning => MsgLevel::Warning,
                core::extensions::UiNotifyLevel::Error => MsgLevel::Error,
            };
            let sys_spans = spans
                .into_iter()
                .map(|s| {
                    let fg = s.fg;
                    SysSpan {
                        fg,
                        bold: false,
                        text: s.text,
                    }
                })
                .collect();
            st.push_msg_rich(sys_spans, level);
        }
        core::extensions::ExtensionUiRequest::ShowDock => {
            st.dock_visible = true;
            st.dock_offset = 0;
            // 空闲时也要立即重绘：`render_tick` 只在 dirty/busy 时画帧，
            // 而工作流/后台代理可能在无流式事件时请求展示（dock 已有内容）。
            st.dirty = true;
        }
        core::extensions::ExtensionUiRequest::CustomMessage {
            label,
            custom_type,
            data,
        } => {
            // 由提供方扩展渲染（找不到认领者时退化为纯文本，避免丢信息）
            let mut spans = vec![SysSpan {
                fg: Some("accent".to_string()),
                bold: true,
                text: format!("[{label}]"),
            }];
            match core::extensions::render_custom_message(&custom_type, &data) {
                Some(rendered) => spans.extend(rendered.into_iter().map(|s| SysSpan {
                    fg: s.fg,
                    bold: false,
                    text: s.text,
                })),
                None => spans.push(SysSpan::plain(format!(" {data}").as_str())),
            }
            st.push_msg_rich(spans, MsgLevel::Info);
        }
        core::extensions::ExtensionUiRequest::ShowOverlay { id } => {
            // 覆盖层与模态面板互斥：`/agents` 一级菜单选中 “workflows / view / create / edit” 后，
            // 覆盖层是下一层视图；一级菜单必须收起，否则会停在覆盖层上方（输入区），出现“第一层 + 覆盖层”同屏。
            st.panel.close();

            // 内容由扩展按 id 拉取；无扩展认领时不打开（避免留下空覆盖层）。
            // 这里拿不到终端宽度（渲染期才有），给一个保守值：扩展只在"两栏"排版上用它，下一帧 `sync` 就会用真实宽度重拉。
            if let Some(view) = core::extensions::overlay_view(id, 80) {
                overlay::open(&mut st, id, view);
            }
        }
        core::extensions::ExtensionUiRequest::HideOverlay { id } => {
            if st.overlay.as_ref().is_some_and(|o| o.id == id) {
                overlay::close(&mut st);
            }
        }
        core::extensions::ExtensionUiRequest::ShowSettings { ext } => {
            st.ext_settings.open(ext);
            st.dirty = true;
        }
        core::extensions::ExtensionUiRequest::Continuation { message } => {
            // 扩展自主续跑：若仍忙碌（user steer/follow-up 已被 apply_sink_agent_settled
            // drained 并启动新一轮）则跳过，避免延续抢跑用户输入；
            // 空闲则入 initial_queue，由本循环尾部 take_next_action 触发下一轮。
            if !st.busy {
                st.initial_queue.push_back(*message);
            }
        }
        core::extensions::ExtensionUiRequest::AbortRun => {
            // 扩展请求中止（如 loop-detect 检测到死循环）：与 Esc 同款硬中止，
            // 但保留用户排队输入（续跑消息随后经 Continuation 正常发送）。
            st.worker.abort();
            apply_hard_abort_state(&mut st);
        }
        core::extensions::ExtensionUiRequest::RewindLastUserInput { restore_to_editor } => {
            // 扩展请求回退最后一次用户输入（/rewind）：记下是否回填编辑器，转 worker 执行；
            // 结果经 on_rewind_done 回执。
            st.pending_rewind_restore = restore_to_editor;
            st.worker.send(agent_actor::AgentCommand::Rewind);
        }
        core::extensions::ExtensionUiRequest::RestartRun { message } => {
            // 扩展请求「中断并重开一轮」（如 hang-detect 检测到假死）：先丢弃进行中的
            // 回合，再把运行中 steer 注入箱里的排队消息与引导消息合成一个 user 批次立即重发。
            st.worker.abort();
            apply_hard_abort_state(&mut st);

            // 引导消息在前，其次是运行中 steer 注入箱（全部）：steer 本就是「运行中注入」的
            // 语义，随新一轮一并送达。**不动**本地 follow-up 队列：follow-up 是「本轮 settle
            // 后才发送」，保持中断前的行为，由新一轮结算后的 drain_next_batch 自然取走。
            let mut batch: Vec<AgentMessage> = vec![*message];

            {
                let mut inbox = st
                    .runtime_steer_inbox
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                batch.extend(inbox.drain(..));
            }

            st.busy = true;
            st.clear_streaming();
            st.status = "Working...".to_string();
            st.working_kind = WorkingKind::Working;
            st.worker.prompt_batch(batch);
        }
        core::extensions::ExtensionUiRequest::RebuildTools => {
            // 扩展工具可见性变化（如 goal 激活/暂停）后重建工具列表，下一轮生效。
            // 忙碌时 worker 会排队，安全。
            st.worker.send(agent_actor::AgentCommand::RebuildTools);
        }
        core::extensions::ExtensionUiRequest::PersistSessionEntry { custom_type, data } => {
            // 扩展持久化：转 worker 空闲追加到当前会话（经 extension:entry_persisted 回执）。
            st.worker.append_custom_entry(&custom_type, Some(data));
        }
    }
    st.dirty = true;
}

/// 会话修复请求处理：启动路径 stage 的损坏会话 → 弹确认面板
/// （复用扩展 Select 面板同款交互：标题 + 选项列表，Enter 确认 / Esc 取消）。
fn maybe_handle_session_repair(shared: &Rc<RefCell<App>>) {
    let Some(req) = take_staged_session_repair() else {
        return;
    };
    shared.borrow_mut().stage_session_repair(req);
}

/// “未完成 operation”提示请求处理：启动路径打开的会话存在 ≥2 个崩溃残留的
/// 未关闭 operation → 弹恢复方式选择面板（Continue / Rewrite / Cancel）。
fn maybe_handle_zombie_operations(shared: &Rc<RefCell<App>>) {
    let Some(req) = take_staged_zombie_operations() else {
        return;
    };
    shared.borrow_mut().stage_zombie_operations(req);
}

/// settings.json 损坏的覆写确认：core 把写入推迟并暂存后，
/// 弹确认面板（Overwrite / Cancel，无输入框）。已有面板/选择器激活时延后到其关闭。
fn maybe_handle_settings_overwrite(shared: &Rc<RefCell<App>>) {
    let mut st = shared.borrow_mut();
    if st.pending_settings_overwrite.is_some()
        || st.panel.active
        || st.settings_selector.active
        || st.session_selector.active
    {
        return;
    }
    let Some((path, detail)) = settings_manager::pending_settings_overwrite() else {
        return;
    };
    st.stage_settings_overwrite(path, detail);
}

/// 空闲时取出待执行动作（prompt / 压缩 / 导航 / 刷新模型）并启动：
/// RefreshModels 自行 spawn 后台任务；其余作为命令下发给 worker（UI 不再持有回合 future）。
fn start_next_action(shared: &Rc<RefCell<App>>, action: NextAction) {
    match action {
        NextAction::Compact(instructions) => {
            let mut st = shared.borrow_mut();
            st.busy = true;
            st.status = "Compacting context... (Esc to cancel)".to_string();
            st.working_kind = WorkingKind::Compaction;
            st.dirty = true;
            drop(st);
            // 回执 compact_done 经闭包投递（on_compact_done 处理含 Nothing/失败提示）
            shared
                .borrow_mut()
                .worker
                .send(agent_actor::AgentCommand::Compact { instructions });
        }
        NextAction::Navigate(target_id, summarize) => {
            let mut st = shared.borrow_mut();
            st.busy = true;
            st.status = if summarize {
                "generating branch summary and navigating...".to_string()
            } else {
                "navigating...".to_string()
            };
            st.working_kind = if summarize {
                WorkingKind::BranchSummary
            } else {
                WorkingKind::Working
            };
            st.dirty = true;
            let w = st.worker.clone();
            drop(st);
            w.send(agent_actor::AgentCommand::Navigate {
                target_id,
                summarize,
            });
        }
        NextAction::Prompt(text) => {
            let mut st = shared.borrow_mut();
            st.busy = true;
            st.clear_streaming();
            st.status = "Working...".to_string();
            st.working_kind = WorkingKind::Working;
            st.dirty = true;
            let w = st.worker.clone();
            drop(st);
            w.prompt(text);
        }
        NextAction::PromptMessage(msg) => {
            let mut st = shared.borrow_mut();
            st.messages.push((*msg).clone());
            st.busy = true;
            st.clear_streaming();
            st.status = "Working...".to_string();
            st.working_kind = WorkingKind::Working;
            st.dirty = true;
            let w = st.worker.clone();
            drop(st);
            w.prompt_message(*msg);
        }
        NextAction::RefreshModels(provider) => {
            // 后台刷新远程模型目录：不阻塞事件循环，结果经闭包投递回主循环。
            // 仅由 /login 成功后触发 → force=true 无视缓存窗立即拉取。
            tokio::spawn(async move {
                let result = model_refresh::refresh_provider_models(&provider, true).await;
                _ = run_in_event_loop(move |ui| {
                    ui.on_catalog_refresh(provider, result.map_err(|e| e.to_string()))
                });
            });
        }
        NextAction::None => {}
    }
}

/// 恢复终端状态
fn teardown_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) {
    crossterm::terminal::disable_raw_mode().ok();
    crossterm::execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        crossterm::event::DisableBracketedPaste,
        crossterm::event::PopKeyboardEnhancementFlags
    )
    .ok();

    // tmux 全量鼠标模式的逆序列 + 恢复 modifyOtherKeys（mode 0）；非 tmux 用 crossterm 全量逆序列
    if in_tmux() {
        _ = write!(
            terminal.backend_mut(),
            "\x1b[>4;0m\x1b[?1006l\x1b[?1004l\x1b[?1003l\x1b[?1002l\x1b[?1000l"
        );
        _ = terminal.backend_mut().flush();
    } else {
        crossterm::execute!(
            terminal.backend_mut(),
            crossterm::event::DisableMouseCapture
        )
        .ok();
    }

    terminal.show_cursor().ok();
}

#[cfg(test)]
mod tests {
    #![allow(clippy::arc_with_non_send_sync)]
    use super::*;
    use crate::modes::interactive::app::App;

    /// 测试 agent 目录守卫：每测试独立临时目录（线程本地 override），
    /// 替代全局锁 + 进程级 env 劫持（并行互不干扰、无死锁）。
    fn test_agent_dir() -> crate::test_support::AgentDirGuard {
        crate::test_support::AgentDirGuard::temp()
    }

    #[test]
    fn startup_model_warning_matches_model_panel_list() {
        // 启动模型可用性提示与 /model 面板列表同口径
        let _g = test_agent_dir();
        let _ad = crate::test_support::AgentDirGuard::temp();
        let _ = crate::core::auth::remove_auth("anthropic").ok();
        let _ = crate::core::auth::remove_auth("deepseek").ok();

        // 无任何凭据：由 init_app 的 “No API keys configured” 覆盖，此处不重复提示
        assert!(startup_model_warning("deepseek", "deepseek-flash", false).is_none());

        // 只配置 anthropic，不配置 deepseek：deepseek 基线目录虽有模型但未登录 → 提示
        crate::core::auth::write_auth_key("anthropic", "sk-ant").unwrap();
        let w = startup_model_warning("deepseek", "deepseek-flash", false);
        assert!(w.is_some());
        let txt = w.unwrap();
        assert!(txt.contains("/login") && txt.contains("/model"));

        // 显式 --api-key 时视为已配置，不再提示
        assert!(startup_model_warning("deepseek", "deepseek-flash", true).is_none());

        // 已配置 provider + 目录中的模型 → 无需提示
        assert!(startup_model_warning("anthropic", "claude-opus-4-8", false).is_none());
        // 已配置 provider + 目录中不存在的模型 → 提示
        assert!(startup_model_warning("anthropic", "no-such-model", false).is_some());

        let _ = crate::core::auth::remove_auth("anthropic").ok();
        let _ = crate::core::auth::remove_auth("deepseek").ok();
    }

    #[test]
    fn project_workflows_only_cwd_opens_startup_trust_panel() {
        // 回归：cwd 只有 `.prux/workflows/*.js`（无 trust.json 条目、defaultProjectTrust=ask）时，
        // 启动必须弹出信任面板，而不是静默把项目工作流丢掉。
        let _ad = test_agent_dir();
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("proj");
        std::fs::create_dir_all(cwd.join(".prux").join("workflows")).unwrap();
        std::fs::write(
            cwd.join(".prux").join("workflows").join("test-simple.js"),
            "export const meta = { name: 'x', description: 'y' };",
        )
        .unwrap();

        let mut st = App::new();
        let cwd_s = cwd.to_string_lossy().to_string();
        st.cwd = cwd_s.clone();
        maybe_ask_project_trust(&mut st, &cwd_s);

        assert!(st.ask_trust, "信任门控应置位（首个消息在决定前不派发）");
        assert_eq!(
            st.panel.top().map(|l| l.kind),
            Some(panel::PanelKind::ProjectTrust),
            "应打开启动信任面板"
        );
    }

    #[test]
    fn command_reply_model_changed_updates_app_mirror() {
        // 场景闭环：worker 回执 model_changed → on_model_changed 更新 App 镜像
        let mut st = App::new();
        st.on_model_changed(Ok(crate::modes::interactive::app::ModelChanged {
            provider: "deepseek".into(),
            model: "v4-pro".into(),
            name: "DeepSeek V4 Pro".into(),
            reasoning: true,
            thinking_level: Some("high".into()),
            thinking_levels: vec!["off".into(), "high".into()],
            persisted: false,
        }));
        assert_eq!(st.current_model.as_deref(), Some("v4-pro"));
        assert_eq!(st.current_provider.as_deref(), Some("deepseek"));
        assert!(st.model_reasoning);
        assert_eq!(st.thinking_level.as_deref(), Some("high"));
        assert_eq!(
            st.thinking_levels,
            vec!["off".to_string(), "high".to_string()]
        );
        let txt = crate::modes::interactive::app::sys_spans_text(&st.system_messages[0].1.spans);
        assert_eq!(txt, "Switched to DeepSeek V4 Pro (deepseek)");
    }

    #[test]
    fn show_dock_request_opens_dock_panel() {
        // 扩展请求 ShowDock → 打开停靠面板（执行计划时自动显示，免手动 /dock；无锚点）。
        // 先清空进程级 UI 请求总线：其它并行测试（如 plan/goal 的 persist()）可能遗留
        // 请求到此共享队列，断言前必须只留本测试自己的请求；持 AUTH 锁与它们串行，
        // 避免清空时把别的用例（后台完成通知等）的请求一并吞掉。
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        while crate::core::extensions::take_pending_ui().is_some() {}
        let st = std::rc::Rc::new(std::cell::RefCell::new(App::new()));
        st.borrow_mut().dirty = false;
        assert!(!st.borrow().dock_visible);
        crate::core::extensions::request_ui(crate::core::extensions::ExtensionUiRequest::ShowDock);
        maybe_handle_extension_ui(&st);
        assert!(st.borrow().dock_visible, "ShowDock 应打开面板");
        assert_eq!(st.borrow().dock_offset, 0, "无锚点从顶部开始");
        assert!(
            st.borrow().dirty,
            "ShowDock 应置 dirty：空闲时（无流式事件）也要立即重绘面板"
        );
    }

    #[test]
    fn restart_run_drains_steer_but_keeps_follow_up_queued() {
        // 假死恢复：RestartRun 先硬中止当前回合，再把运行中 steer 与引导消息
        // 合成一批重开一轮；follow-up 保持「settle 后才发送」的语义，留在队列里不动。
        // 共享队列：与其它取用/清空它的用例串行（同 `show_dock_request_opens_dock_panel`）。
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        while crate::core::extensions::take_pending_ui().is_some() {}
        let st = std::rc::Rc::new(std::cell::RefCell::new(App::new()));
        {
            let mut s = st.borrow_mut();
            s.busy = true;
            s.runtime_steer_inbox
                .lock()
                .unwrap()
                .push_back(AgentMessage::user_text("steer me"));
            s.runtime_followup_inbox.push_back("follow me".to_string());
        }
        crate::core::extensions::request_ui(
            crate::core::extensions::ExtensionUiRequest::RestartRun {
                message: Box::new(AgentMessage::user_text("nudge")),
            },
        );
        maybe_handle_extension_ui(&st);

        let s = st.borrow();
        assert!(s.busy, "重开后应重新进入忙碌态");
        assert!(
            s.runtime_steer_inbox.lock().unwrap().is_empty(),
            "steer 应随新一轮一并发送"
        );
        assert_eq!(
            s.runtime_followup_inbox,
            std::collections::VecDeque::from(vec!["follow me".to_string()]),
            "follow-up 应留在队列里，等新一轮 settle 后自然发送"
        );
        assert!(
            s.messages
                .iter()
                .any(|m| m.stop_reason.as_deref() == Some("aborted")),
            "被丢弃的回合应补写中断提示"
        );
    }

    #[test]
    fn command_reply_session_switched_updates_app_mirror() {
        // `on_session_switched` 经扩展注册表分发（subagent → manager::reset_all），
        // 必须与触碰 subagent 全局态的测试串行。
        let _sub = crate::extensions::subagent::test_lock();
        // 场景闭环：resume 回执 session_switched → on_session_switched 更新消息流与会话路径（含旧提示清除）；
        // 会话切换同时隐藏停靠面板（/new /resume /import /fork /clone 统一钩子）
        let mut st = App::new();
        st.dock_visible = true;
        st.dock_offset = 3;
        st.system_messages.push((
            1,
            crate::modes::interactive::app::SysMsg::new(
                MsgLevel::Info,
                vec![crate::modes::interactive::app::SysSpan::plain("stale")],
            ),
        ));
        st.on_session_switched(Ok(crate::modes::interactive::app::SessionSwitched {
            session_id: "abc".into(),
            name: None,
            file: Some("/tmp/s.json".into()),
            messages: vec![AgentMessage::user_text("hi")],
        }));
        assert_eq!(st.current_session_path.as_deref(), Some("/tmp/s.json"));
        assert!(st.system_messages.is_empty(), "切换后旧提示应清除");
        assert_eq!(st.messages.len(), 1);
        assert_eq!(st.messages[0].text(), "hi");
        assert!(!st.dock_visible, "会话切换应隐藏停靠面板");
        assert_eq!(st.dock_offset, 0, "隐藏时重置偏移");
    }

    #[test]
    fn session_switched_refreshes_context_percent() {
        // `on_session_switched` 经扩展注册表分发（subagent → manager::reset_all）。
        let _sub = crate::extensions::subagent::test_lock();
        // 会话切换（/new /resume /import /fork /clone 汇合点）后 context 使用率
        // 必须按新消息集刷新：残留旧会话余额会在底栏一直显示到首条消息到达
        let mut st = App::new();
        st.context_window = 100_000;
        st.context_percent = 97.0; // 旧会话余额
        st.on_session_switched(Ok(crate::modes::interactive::app::SessionSwitched {
            session_id: "abc".into(),
            name: None,
            file: Some("/tmp/s.json".into()),
            messages: vec![AgentMessage::user_text("hello, new session body")],
        }));
        assert!(
            st.context_percent < 97.0 && st.context_percent > 0.0,
            "切换后应按新消息集刷新使用率: {}",
            st.context_percent
        );
    }

    #[test]
    fn cancel_prompt_pushes_aborted_hint_message() {
        // Esc/Ctrl+C 硬中止：worker drop prompt future 后 agent 不再吐 agent_end，
        // handle_cancel_prompt 必须补写 stop_reason=aborted 消息，消息区才有中断提示
        let shared = Rc::new(RefCell::new(App::new()));
        {
            let mut st = shared.borrow_mut();
            st.busy = true;
            st.streaming_text.push_str("partial output");
        }
        assert!(
            !process_key_action(&shared, KeyAction::CancelPrompt),
            "取消不退出"
        );
        let st = shared.borrow();
        assert!(!st.busy, "取消后 busy 复位");
        assert!(st.streaming_text.is_empty(), "流式缓冲清空");
        let last = st.messages.last().expect("应有 aborted 消息");
        assert_eq!(last.role, "assistant");
        assert_eq!(last.stop_reason.as_deref(), Some("aborted"));
        assert_eq!(last.error_message.as_deref(), Some("Operation aborted"));
        assert!(st.stream_anchor_index.is_none(), "流式锚点应失效");

        // 渲染确认：中断提示出现在消息区（红色行 “Operation aborted”）
        let texts = crate::modes::interactive::render::render_messages_text(&st, 80);
        assert!(
            texts.iter().any(|l| l.contains("Operation aborted")),
            "中断提示应出现在消息区: {texts:?}"
        );
    }

    #[test]
    fn cancel_prompt_stops_elapsed_timer_on_tool_block() {
        // Esc/Ctrl+C 硬中止：进行中工具的 started_at 必须清空，否则工具块命中缓存
        // 继续渲染/刷新 `Elapsed X.Xs`（Esc 后仍在计时），且不许再强制重建。
        let shared = Rc::new(RefCell::new(App::new()));
        let before_started: u64 = {
            let mut st = shared.borrow_mut();
            st.busy = true;
            // 造一条含进行中工具调用的 assistant 消息（结果未落地）
            let mut m = AgentMessage::user_text("");
            m.role = "assistant".to_string();
            m.content = vec![crate::ContentBlock::ToolCall {
                id: "call_1".to_string(),
                name: "bash".to_string(),
                arguments: serde_json::json!({ "command": "sleep 5" }),
                thought_signature: None,
                namespace: None,
            }];
            st.messages.push(m);
            st.tool_started_at
                .insert("call_1".to_string(), crate::utils::time::now_ms() - 3000);
            // 先渲染一次：缓存条目带 started_at（强制重建条件成立，渲染 Elapsed）
            let r = st.render_history_lines(80);
            r.segments
                .iter()
                .find_map(|(_, _, p)| p.iter().find_map(|p| p.started_at_ms))
                .expect("进行中工具块应携带 started_at")
        };
        assert!(before_started > 0);

        // Esc 取消：清理计时状态
        process_key_action(&shared, KeyAction::CancelPrompt);
        {
            let st = shared.borrow();
            assert!(
                !st.tool_started_at.contains_key("call_1"),
                "取消后进行中工具计时必须清空，否则 Elapsed 持续刷新"
            );
            assert!(
                st.last_elapsed_rebuild.is_none(),
                "取消后应允许下一帧强制重渲染以去除 Elapsed"
            );
        }
        {
            // 重渲染确认：工具块不再携带 started_at（Elapsed 分支不再生效）
            let mut st = shared.borrow_mut();
            let r2 = st.render_history_lines(80);
            let any_started = r2
                .segments
                .iter()
                .any(|(_, _, p)| p.iter().any(|p| p.started_at_ms.is_some()));
            assert!(!any_started, "取消后工具块不应再驱动 Elapsed 渲染");
        }
    }

    #[test]
    fn drain_next_batch_steer_before_follow_up() {
        // steer 在运行中注入箱、follow-up 在本地队列；出队 steer 优先于 follow-up
        // （对齐 pi 先 drain steeringQueue 再 drain followUpQueue），同类内部 FIFO。
        // 默认 one-at-a-time：每次只取一条，剩余保留下轮再取。
        let _g = test_agent_dir();
        // 显式立好模式，不依赖共享 settings.json 的残留值（并行测试可能留下 all）
        crate::core::settings_manager::write_settings_steering_mode("one-at-a-time").unwrap();
        crate::core::settings_manager::write_settings_follow_up_mode("one-at-a-time").unwrap();
        let shared = Rc::new(RefCell::new(App::new()));
        {
            let mut st = shared.borrow_mut();
            st.runtime_steer_inbox
                .lock()
                .unwrap()
                .push_back(crate::core::provider::AgentMessage::user_text("steer one"));
            st.runtime_steer_inbox
                .lock()
                .unwrap()
                .push_back(crate::core::provider::AgentMessage::user_text("steer two"));
            st.runtime_followup_inbox.push_back("fup one".to_string());
        }
        // steer 两条先出（各自 FIFO，逐条），follow-up 最后出
        let mut st = shared.borrow_mut();
        assert_eq!(drain_next_batch(&mut st).unwrap()[0].text(), "steer one");
        assert_eq!(drain_next_batch(&mut st).unwrap()[0].text(), "steer two");
        assert_eq!(drain_next_batch(&mut st).unwrap()[0].text(), "fup one");
        assert!(drain_next_batch(&mut st).is_none());
    }

    #[test]
    fn drain_next_batch_all_empties_whole_batch() {
        // steering mode=all：一批取走全部相同类（对齐 pi PendingMessageQueue.drain all）
        let _g = test_agent_dir();
        let shared = Rc::new(RefCell::new(App::new()));
        crate::core::settings_manager::write_settings_steering_mode("all").unwrap();
        crate::core::settings_manager::write_settings_follow_up_mode("all").unwrap();
        {
            let mut st = shared.borrow_mut();
            st.runtime_steer_inbox
                .lock()
                .unwrap()
                .push_back(crate::core::provider::AgentMessage::user_text("s1"));
            st.runtime_steer_inbox
                .lock()
                .unwrap()
                .push_back(crate::core::provider::AgentMessage::user_text("s2"));
            st.runtime_followup_inbox.push_back("f1".to_string());
            st.runtime_followup_inbox.push_back("f2".to_string());
        }
        // all：steer 一批全取，然后 follow-up 一批全取
        let mut st = shared.borrow_mut();
        let s = drain_next_batch(&mut st).unwrap();
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].text(), "s1");
        assert_eq!(s[1].text(), "s2");
        let f = drain_next_batch(&mut st).unwrap();
        assert_eq!(f.len(), 2);
        assert_eq!(f[0].text(), "f1");
        assert_eq!(f[1].text(), "f2");
        assert!(drain_next_batch(&mut st).is_none());
        // 复位默认
        crate::core::settings_manager::write_settings_steering_mode("one-at-a-time").unwrap();
        crate::core::settings_manager::write_settings_follow_up_mode("one-at-a-time").unwrap();
    }
}
