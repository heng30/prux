//! `recap` 扩展：停靠面板里的一行会话小结（`/recap [now|on|off]`）。
//!
//! 在状态区显示一行 `※ recap: …`（≤120 字），让"切走一会儿再回来"的人一眼看到会话在干什么。
//!
//! 小结走一次**独立的一次性请求**（最近 [`EXCERPT_MESSAGES`] 条消息、
//! [`EXCERPT_CHARS`] 字以内的摘录，`max_tokens` 限 [`MAX_TOKENS`]），
//! 结果规整到 [`MAX_CHARS`] 字以内（去前缀、去引号、硬截断）。
//!
//! 默认开启自动小结：空闲 [`IDLE_RECAP`] 后自动重刷一次，`on` / `off` 切换并把
//! `autoRecap` 写进 `<agent_dir>/extensions/recap.json`（也可在扩展设置面板里改）。
//! 空闲判定由一个 [`TICK`] 一拍的后台任务做，代际机制与 `subagent::schedule` 相同。
//!
//! **扩展本身默认不启用**，且只在 `Dev` / `Creator` 扩展模式下可用——开启后空闲就会
//! 发起模型请求，该由用户自己决定。

use super::{config, shared};
use crate::{
    core::{
        extensions::{
            DockLine, DockSpan, Extension, ExtensionCommand, ExtensionHook, ExtensionMode,
            ExtensionSetting, ExtensionTool, SubcommandDef, UiNotifyLevel, is_extension_active,
            request_show_dock,
        },
        provider::AgentMessage,
        session_manager::Session,
    },
    extensions::{
        EXTENSION_FACTORIES, ExtensionFactory, MUTED, PRIORITY_RECAP, command_arg,
        util::{notify_text, truncate},
    },
    modes::interactive::{
        app::{App, MsgLevel},
        handlers::register_slash_command,
    },
    utils::glyphs::DEF_RECAP,
};
use std::{
    sync::{Arc, Mutex, MutexGuard, OnceLock},
    time::{Duration, Instant},
};

/// 扩展名。
const EXT: &str = "recap";

/// 系统提示：压成一行、给"回来接着干"的人看。
const SYSTEM: &str = "Summarize this coding session in ONE line of at most 120 characters. \
                      Say what is being worked on and where it stands. No preamble, no \
                      markdown heading, no trailing period padding.";

/// 摘录的消息条数上限。
const EXCERPT_MESSAGES: usize = 12;

/// 摘录的字符上限。
const EXCERPT_CHARS: usize = 12000;

/// 回复的 token 上限。
const MAX_TOKENS: u32 = 160;

/// 一行小结的字符上限
const MAX_CHARS: usize = 120;

/// 空闲多久后自动重刷小结。
const IDLE_RECAP: Duration = Duration::from_secs(300);

/// 空闲判定的 tick 间隔。
const TICK: Duration = Duration::from_secs(30);

/// `/recap` 的子命令（顺序 = 输入框候选顺序）。
const SUBCOMMANDS: &[SubcommandDef] = &[
    SubcommandDef {
        name: "now",
        description: "Refresh the one-line recap now",
    },
    SubcommandDef {
        name: "on",
        description: "Enable the automatic recap after 5 minutes idle",
    },
    SubcommandDef {
        name: "off",
        description: "Disable the automatic recap",
    },
];

/// 自声明工厂：linkme 分布式切片。
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static RECAP_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_RECAP,
    make: || -> Arc<dyn Extension> { Arc::new(Recap) },
};

/// 扩展状态（进程级单例）。
struct State {
    /// 当前小结（None = 还没有）。
    recap: Option<String>,
    /// 小结是否正在生成。
    recap_loading: bool,
    /// 异步结果刚落地、还没渲染成帧（true = 下一帧必须重绘）。
    ///
    /// 空闲时 TUI 只在 80ms tick 里轮询 [`Extension::wants_redraw`]：生成中它为真，
    /// 结果一落地若直接变假，那一帧就永远不会被画出来（要等按键/鼠标事件才刷新），
    /// 表现为 `/recap now` “没效果”（与 `btw` 同一个坑）。
    refresh_pending: bool,
    /// 自动小结开关（默认开，与 kiss 一致）。
    auto_recap: bool,
    /// 设置文件是否已读过（每会话读一次）。
    settings_loaded: bool,
    /// 最近一次转录摘录（每次 LLM 调用前刷新）。
    excerpt: String,
    /// 摘录里最新的 `(provider, model)`，自动小结就用它。
    model: Option<(String, String)>,
    /// 最近一次活动（用户提交或模型调用）的时间。
    last_activity: Instant,
    /// 最近一次生成小结的时间。
    last_recap: Option<Instant>,
    /// tick 任务的"代"：会话切换或禁用时 +1，旧任务醒来后自己退出。
    ticker_gen: u64,
}

impl Default for State {
    /// 默认状态：自动小结开启，活动时间从此刻算起。
    fn default() -> Self {
        Self {
            recap: None,
            recap_loading: false,
            refresh_pending: false,
            auto_recap: true,
            settings_loaded: false,
            excerpt: String::new(),
            model: None,
            last_activity: Instant::now(),
            last_recap: None,
            ticker_gen: 0,
        }
    }
}

/// 取扩展状态的进程级单例（惰性初始化）。
fn state() -> &'static Mutex<State> {
    /// 扩展状态的进程级单例。
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(State::default()))
}

/// 取状态锁；锁被毒化时取回内部值。
fn lock() -> MutexGuard<'static, State> {
    state().lock().unwrap_or_else(|e| e.into_inner())
}

/// `recap` 扩展。
pub struct Recap;

impl Extension for Recap {
    /// 扩展名 `recap`。
    fn name(&self) -> &str {
        EXT
    }

    /// 一句话说明：一行会话小结。
    fn description(&self) -> &str {
        "One-line session recap in the dock (/recap [now|on|off]); refreshes after 5 minutes idle"
    }

    /// 默认**不启用**：开启后空闲即自动发起小结请求（要花钱），不该由框架替用户决定。
    fn default_enabled(&self) -> bool {
        false
    }

    /// 只在 `Dev` / `Creator` 扩展模式下可用。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Dev, ExtensionMode::Creator]
    }

    /// 不提供模型工具（小结由用户或空闲触发）。
    fn tools(&self) -> Vec<ExtensionTool> {
        Vec::new()
    }

    /// 声明 `/recap` 命令及 `now` / `on` / `off` 子命令。
    fn commands(&self) -> Vec<ExtensionCommand> {
        vec![ExtensionCommand {
            name: "recap".to_string(),
            description:
                "/recap [now|on|off] (one-line session summary; auto after 5 minutes idle)"
                    .to_string(),
            busy_safe: true, // 只起一次性请求，不锁 agent
            subcommands: SUBCOMMANDS.to_vec(),
        }]
    }

    /// 注册 `/recap` 的 handler。
    fn on_registered(&self) {
        register_slash_command(EXT, "recap", command_recap);
    }

    /// 会话建立：读一次设置、重置跨会话状态、起 tick 任务。
    fn on_session_start(&self, _cwd: &str, _messages: &[AgentMessage], session: Option<&Session>) {
        shared::remember_session(session.map(|s| s.session_id.clone()));
        reset_for_session();
    }

    /// 会话切换：清掉上一条会话的小结并重新起 tick。
    fn on_session_switched(&self, session_path: Option<&str>, messages: &[AgentMessage]) {
        shared::remember_session(session_id_of(session_path));
        let excerpt = shared::build_excerpt(messages, EXCERPT_MESSAGES, EXCERPT_CHARS);
        let model = shared::newest_model(messages);
        {
            let mut st = lock();
            st.recap = None;
            st.recap_loading = false;
            st.refresh_pending = false;
            st.last_recap = None;
            st.excerpt = excerpt;
            st.model = model;
            st.last_activity = Instant::now();
        }
        start_ticker();
    }

    /// 禁用时清掉小结，并让 tick 任务退休。
    fn on_enabled_changed(&self, enabled: bool) {
        if !enabled {
            let mut st = lock();
            st.ticker_gen = st.ticker_gen.wrapping_add(1);
            st.recap = None;
            st.recap_loading = false;
            st.refresh_pending = false;
        }
    }

    /// 关心停靠面板（小结）、上下文变换（刷新摘录）与用户提交（活动时间）。
    fn hooks(&self) -> Vec<ExtensionHook> {
        let st = lock();
        let mut hooks = vec![ExtensionHook::TransformContext, ExtensionHook::UserPrompt];
        if st.recap_loading || st.recap.is_some() {
            hooks.push(ExtensionHook::Dock);
        }
        hooks
    }

    /// 生成中要重绘（停靠面板的 spinner）；结果落地后再补一帧。
    fn wants_redraw(&self) -> bool {
        let st = lock();
        st.recap_loading || st.refresh_pending
    }

    /// 每次 LLM 调用前刷新摘录快照：自动小结从这份快照取上下文。
    fn transform_context(&self, messages: &mut Vec<AgentMessage>) -> crate::error::Result<()> {
        let excerpt = shared::build_excerpt(messages, EXCERPT_MESSAGES, EXCERPT_CHARS);
        let model = shared::newest_model(messages);
        let mut st = lock();
        st.excerpt = excerpt;
        if model.is_some() {
            st.model = model;
        }
        st.last_activity = Instant::now();
        Ok(())
    }

    /// 用户提交新输入 = 有活动（空闲计时归零）。
    fn on_user_submit(&self, _text: &str, _busy: bool) {
        lock().last_activity = Instant::now();
    }

    /// 停靠面板：一行 `※ recap: …`（生成中显示 spinner）。
    fn dock_lines(&self) -> Vec<DockLine> {
        let mut st = lock();
        // 内容即将被渲染：这一帧之后不再为此额外请求重绘
        st.refresh_pending = false;
        if st.recap_loading {
            return vec![vec![DockSpan::new(
                MUTED.0,
                MUTED.1,
                format!(
                    "{} creating recap…",
                    crate::extensions::util::spinner_frame()
                ),
            )]];
        }
        match &st.recap {
            Some(text) => vec![vec![DockSpan::new(
                MUTED.0,
                MUTED.1,
                format!("{DEF_RECAP} recap: {text}"),
            )]],
            None => Vec::new(),
        }
    }

    /// 扩建设置面板里的一项：自动小结开关。
    fn settings(&self) -> Vec<ExtensionSetting> {
        let auto = lock().auto_recap;
        vec![ExtensionSetting {
            key: "autoRecap".to_string(),
            label: "Auto recap".to_string(),
            description: "Refresh the one-line recap after 5 minutes idle".to_string(),
            value: if auto { "on" } else { "off" }.to_string(),
            choices: vec!["off".to_string(), "on".to_string()],
        }]
    }

    /// 应用设置面板的变更并落盘。
    fn apply_setting(&self, key: &str, value: &str) -> Result<(), String> {
        if key != "autoRecap" {
            return Err(format!("unknown setting: {key}"));
        }
        let on = match value {
            "on" => true,
            "off" => false,
            other => return Err(format!("invalid value for autoRecap: {other}")),
        };
        lock().auto_recap = on;
        config::set_auto_recap(on)
    }
}

/// 会话建立 / 切换后重置状态并启动 tick 任务。
fn reset_for_session() {
    {
        let mut st = lock();
        if !st.settings_loaded {
            st.auto_recap = config::auto_recap();
            st.settings_loaded = true;
        }
        st.recap = None;
        st.recap_loading = false;
        st.refresh_pending = false;
        st.last_recap = None;
        st.last_activity = Instant::now();
    }
    start_ticker();
}

/// 会话文件路径 → 会话 id（文件名去扩展名）。
fn session_id_of(session_path: Option<&str>) -> Option<String> {
    session_path
        .and_then(|p| std::path::Path::new(p).file_stem())
        .map(|s| s.to_string_lossy().to_string())
}

/// 启动 30 秒 tick 的空闲检查任务。
///
/// 用"代"而不是布尔：会话切换时旧任务可能还在睡（最多 30 秒），布尔会让新会话永远起不来；
/// 旧任务发现自己的代过期或扩展被禁用就退出。
fn start_ticker() {
    let generation = {
        let mut st = lock();
        st.ticker_gen = st.ticker_gen.wrapping_add(1);
        st.ticker_gen
    };
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return; // 没有运行时（同步单测）：下次会话建立再试
    };

    handle.spawn(async move {
        loop {
            tokio::time::sleep(TICK).await;
            let current = lock().ticker_gen;
            if current != generation || !is_extension_active(EXT) {
                return;
            }
            maybe_start_recap();
        }
    });
}

/// 空闲判定通过时发起一次自动小结；返回是否发起了。
fn maybe_start_recap() -> bool {
    let (excerpt, model) = {
        let mut st = lock();
        let due = should_start_recap(
            st.last_activity.elapsed(),
            st.last_recap.map(|t| t.elapsed()),
            st.auto_recap,
            st.model.is_some(),
            !st.excerpt.trim().is_empty(),
        );
        if !due || st.recap_loading {
            return false;
        }
        st.recap_loading = true;
        st.last_recap = Some(Instant::now());
        (st.excerpt.clone(), st.model.clone())
    };

    let Some((provider, model_id)) = model else {
        lock().recap_loading = false;
        return false;
    };

    // 停靠面板默认隐藏：自动小结同样要主动请求打开，否则小结看不见
    request_show_dock();
    request_recap(provider, model_id, excerpt)
}

/// 纯判定：是否该发起自动小结。
///
/// `idle` = 距上次活动的时间；`since_recap` = 距上次小结的时间（None = 从未小结）。
fn should_start_recap(
    idle: Duration,
    since_recap: Option<Duration>,
    auto: bool,
    has_model: bool,
    has_excerpt: bool,
) -> bool {
    if !auto || !has_model || !has_excerpt {
        return false;
    }
    if idle < IDLE_RECAP {
        return false;
    }
    since_recap.is_none_or(|elapsed| elapsed >= IDLE_RECAP)
}

/// `/recap` handler：`now` 立即生成，`on` / `off` 切换自动小结。
fn command_recap(st: &mut App, raw: &str) -> bool {
    // 本会话中途启用时会话钩子没跑过，槽里没有 id（opencode 会报缺 x-opencode-session）
    shared::refresh_session_from_app(st);

    match command_arg(raw).trim() {
        "" | "now" => {
            if let Some((provider, model_id, excerpt)) = recap_input(st) {
                prepare_recap();
                request_recap(provider, model_id, excerpt);
            }
        }
        "on" => {
            lock().auto_recap = true;
            match config::set_auto_recap(true) {
                Ok(()) => st.push_msg("Automatic recap is on.".to_string(), MsgLevel::Info),
                Err(e) => st.push_msg(format!("failed to save autoRecap: {e}"), MsgLevel::Warning),
            }
        }
        "off" => {
            let mut state = lock();
            state.auto_recap = false;
            state.recap_loading = false;
            state.refresh_pending = false;
            drop(state);
            match config::set_auto_recap(false) {
                Ok(()) => st.push_msg("Automatic recap is off.".to_string(), MsgLevel::Info),
                Err(e) => st.push_msg(format!("failed to save autoRecap: {e}"), MsgLevel::Warning),
            }
        }
        other => st.push_msg(
            format!("Unknown /recap subcommand: {other}. Use `now`, `on` or `off`."),
            MsgLevel::Warning,
        ),
    }
    false
}

/// 手动小结需要的输入：`(provider, model, 摘录)`；缺任何一项时提示并返回 `None`。
fn recap_input(st: &mut App) -> Option<(String, String, String)> {
    let snapshot = lock().model.clone();
    let Some((provider, model_id)) = shared::current_model(st, snapshot) else {
        st.push_msg(
            "No model is selected yet. Pick one with /model, then try again.".to_string(),
            MsgLevel::Warning,
        );
        return None;
    };

    // 命令从 TUI 发起：直接读渲染历史，比等下一次 LLM 调用刷新快照更准
    let excerpt = if st.messages.is_empty() {
        lock().excerpt.clone()
    } else {
        shared::build_excerpt(&st.messages, EXCERPT_MESSAGES, EXCERPT_CHARS)
    };

    if excerpt.trim().is_empty() {
        st.push_msg(
            "Nothing to summarize yet — this session has no messages.".to_string(),
            MsgLevel::Info,
        );
        return None;
    }

    Some((provider, model_id, excerpt))
}

/// 标记小结开始（停靠面板立即切到 spinner）。
///
/// 停靠面板默认是隐藏的（`dock_visible = false`），有内容要展示时得主动请求打开，
/// 否则 `dock_lines` 根本不会被调用（与 plan-mode 用 `request_show_dock` 同款）。
fn prepare_recap() {
    {
        let mut st = lock();
        st.recap_loading = true;
        st.last_recap = Some(Instant::now());
    }
    request_show_dock();
}

/// 发起一次小结请求。
fn request_recap(provider: String, model_id: String, excerpt: String) -> bool {
    if let Err(e) =
        shared::spawn_one_shot(provider, model_id, SYSTEM, excerpt, MAX_TOKENS, |result| {
            let mut st = lock();
            match result {
                Ok(text) => {
                    let updated = st.recap_loading;
                    if updated {
                        st.recap = Some(normalize_recap(&text));
                        st.recap_loading = false;
                        // 结果落地后要再补一帧，否则空闲时这一帧永远不会被画出来
                        st.refresh_pending = true;
                    }
                    drop(st);

                    // 面板可能在生成期间被关掉：有内容就再请求打开
                    if updated {
                        request_show_dock();
                    }
                }
                Err(e) => {
                    let failed = st.recap_loading;
                    st.recap_loading = false;
                    drop(st);
                    if failed {
                        notify_text(&format!("recap failed: {e}"), UiNotifyLevel::Warning);
                    }
                }
            }
        })
    {
        lock().recap_loading = false;
        notify_text(&format!("recap failed: {e}"), UiNotifyLevel::Warning);
        return false;
    }
    true
}

/// 规整小结文本：去掉前缀（`recap:` / `※`）与引号、归一空白，再按字符上限硬截断
/// （契约是"≤ [`MAX_CHARS`] 字"，省略号不该占配额）。
fn normalize_recap(text: &str) -> String {
    let mut cleaned = text.trim().to_string();
    for prefix in [
        DEF_RECAP, "recap:", "Recap:", "recap :", "SUMMARY:", "Summary:",
    ] {
        if let Some(rest) = cleaned.strip_prefix(prefix) {
            cleaned = rest.trim().to_string();
        }
    }
    cleaned = cleaned
        .trim_matches(|c| c == '"' || c == '\'' || c == '“' || c == '”')
        .trim()
        .to_string();
    cleaned = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate(&cleaned, MAX_CHARS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        core::{extensions::ExtensionUiRequest, provider::AgentMessage},
        test_support::{AUTH_TEST_LOCK, AgentDirGuard},
    };

    /// 全局状态锁（多个用例共享进程级 state 与 UI 队列）。
    fn lock_global() -> MutexGuard<'static, ()> {
        AUTH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 一条指定角色的消息。
    fn message(role: &str, text: &str) -> AgentMessage {
        let mut message = AgentMessage::user_text(text);
        message.role = role.to_string();
        message
    }

    /// 一条带 model/provider 的 assistant 消息。
    fn assistant(text: &str, provider: &str, model_id: &str) -> AgentMessage {
        let mut message = message("assistant", text);
        message.provider = Some(provider.to_string());
        message.model = Some(model_id.to_string());
        message
    }

    /// 复位扩展状态。
    fn reset_state() {
        let mut st = lock();
        *st = State::default();
        st.settings_loaded = true;
    }

    /// 停靠行拼成一段文本。
    fn dock_text(ext: &Recap) -> String {
        ext.dock_lines()
            .iter()
            .map(|line| line.iter().map(|s| s.text.clone()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn normalize_recap_strips_prefix_quotes_and_caps_length() {
        assert_eq!(
            normalize_recap("  Recap: working on the parser  "),
            "working on the parser"
        );
        assert_eq!(normalize_recap("※ \"done\" "), "done");
        assert_eq!(normalize_recap("a\n  b"), "a b", "换行与多空格归一");
        let long = normalize_recap(&"字".repeat(200));
        assert_eq!(long.chars().count(), MAX_CHARS, "硬截断到上限");
    }

    #[test]
    fn should_start_recap_respects_idle_auto_and_inputs() {
        let idle = IDLE_RECAP + Duration::from_secs(1);
        assert!(should_start_recap(idle, None, true, true, true));
        assert!(
            !should_start_recap(idle, None, false, true, true),
            "关掉就不触发"
        );
        assert!(
            !should_start_recap(idle, None, true, false, true),
            "没有型号不触发"
        );
        assert!(
            !should_start_recap(idle, None, true, true, false),
            "没摘录不触发"
        );
        assert!(
            !should_start_recap(IDLE_RECAP - Duration::from_secs(1), None, true, true, true),
            "没到空闲时间不触发"
        );
        assert!(
            !should_start_recap(idle, Some(Duration::from_secs(10)), true, true, true),
            "刚小结过不重复触发"
        );
        assert!(should_start_recap(
            idle,
            Some(IDLE_RECAP + Duration::from_secs(1)),
            true,
            true,
            true
        ));
    }

    #[test]
    fn dock_lines_render_spinner_then_recap() {
        let _g = lock_global();
        reset_state();
        let ext = Recap;
        assert!(ext.dock_lines().is_empty(), "没有小结时不占停靠面板");

        lock().recap_loading = true;
        assert!(
            dock_text(&ext).contains("creating recap"),
            "生成中显示 spinner"
        );
        assert!(ext.wants_redraw(), "生成中要重绘");

        lock().recap_loading = false;
        lock().recap = Some("working on the parser".to_string());
        assert_eq!(
            dock_text(&ext),
            format!("{DEF_RECAP} recap: working on the parser")
        );
        assert!(!ext.wants_redraw(), "静态小结不需要每帧重绘");
        assert!(ext.hooks().contains(&ExtensionHook::Dock));
    }

    /// 小结结果落地后必须再补一帧（否则空闲时停靠面板永远看不到小结）。
    /// 停靠面板默认隐藏：`/recap now` 开始生成时必须请求打开（否则小结看不见）。
    #[test]
    fn recap_now_requests_the_dock_panel() {
        let _g = lock_global();
        reset_state();
        while crate::core::extensions::take_pending_ui().is_some() {}

        let mut st = App::new();
        st.current_provider = Some("anthropic".to_string());
        st.current_model = Some("claude-fable-5".to_string());
        st.messages.push(message("user", "hello world"));

        assert!(!command_recap(&mut st, "recap now"));
        assert!(
            matches!(
                crate::core::extensions::take_pending_ui(),
                Some(ExtensionUiRequest::ShowDock)
            ),
            "开始生成就应请求打开停靠面板"
        );
        // 单测没有 tokio 运行时 → 请求失败会投一条通知；清掉，免得污染其它用例
        while crate::core::extensions::take_pending_ui().is_some() {}
    }

    #[test]
    fn recap_landing_requests_one_more_frame() {
        let _g = lock_global();
        reset_state();
        let ext = Recap;

        lock().recap_loading = true;
        assert!(ext.wants_redraw(), "生成中要重绘");

        // 异步结果落地：loading 转假、recap 就绪、refresh_pending 置位
        {
            let mut st = lock();
            st.recap_loading = false;
            st.recap = Some("done".to_string());
            st.refresh_pending = true;
        }
        assert!(ext.wants_redraw(), "结果刚落地的下一帧必须重绘");
        assert!(dock_text(&ext).contains("done"), "这一帧应画出小结");
        assert!(!ext.wants_redraw(), "渲染过一帧后不再持续重绘");
    }

    #[test]
    fn recap_subcommands_toggle_and_persist() {
        let _g = lock_global();
        let _ad = AgentDirGuard::temp();
        reset_state();
        let mut st = App::new();

        assert!(!command_recap(&mut st, "recap on"));
        assert!(lock().auto_recap);
        assert!(config::auto_recap(), "on 应落盘");

        let mut st = App::new();
        assert!(!command_recap(&mut st, "recap off"));
        assert!(!lock().auto_recap);
        assert!(!config::auto_recap(), "off 应落盘");

        let mut st = App::new();
        assert!(!command_recap(&mut st, "recap bogus"));
        assert!(!st.system_messages.is_empty(), "未知子命令应提示用法");
    }

    #[test]
    fn recap_now_without_model_or_history_hints() {
        let _g = lock_global();
        reset_state();
        let mut st = App::new();
        st.current_provider = None;
        st.current_model = None;
        assert!(!command_recap(&mut st, "recap now"));
        assert!(!st.system_messages.is_empty(), "没有型号应提示");

        let mut st = App::new();
        st.current_provider = Some("anthropic".to_string());
        st.current_model = Some("claude-fable-5".to_string());
        assert!(!command_recap(&mut st, "recap now"));
        assert!(!st.system_messages.is_empty(), "没有历史应提示");
    }

    #[test]
    fn apply_setting_validates_and_persists() {
        let _g = lock_global();
        let _ad = AgentDirGuard::temp();
        reset_state();
        let ext = Recap;

        assert!(ext.apply_setting("autoRecap", "off").is_ok());
        assert!(!lock().auto_recap);
        assert!(!config::auto_recap());
        assert!(ext.apply_setting("autoRecap", "on").is_ok());
        assert!(config::auto_recap());
        assert!(ext.apply_setting("autoRecap", "maybe").is_err());
        assert!(ext.apply_setting("unknown", "on").is_err());
    }

    #[test]
    fn settings_panel_reports_current_value() {
        let _g = lock_global();
        reset_state();
        lock().auto_recap = false;
        let settings = Recap.settings();
        assert_eq!(settings.len(), 1);
        assert_eq!(settings[0].key, "autoRecap");
        assert_eq!(settings[0].value, "off");
        assert_eq!(settings[0].choices, vec!["off", "on"]);
    }

    #[test]
    fn transform_context_snapshots_excerpt_and_model() {
        let _g = lock_global();
        reset_state();
        let mut messages = vec![
            message("user", "do the thing"),
            assistant("done", "anthropic", "claude-fable-5"),
        ];
        Recap.transform_context(&mut messages).unwrap();
        let st = lock();
        assert_eq!(st.excerpt, "user: do the thing\nassistant: done");
        assert_eq!(
            st.model,
            Some(("anthropic".to_string(), "claude-fable-5".to_string()))
        );
    }

    #[test]
    fn disabled_extension_clears_state_and_hooks() {
        let _g = lock_global();
        reset_state();
        {
            let mut st = lock();
            st.recap = Some("x".to_string());
            st.recap_loading = true;
        }
        assert!(Recap.wants_redraw());

        Recap.on_enabled_changed(false);
        let st = lock();
        assert!(st.recap.is_none());
        assert!(!st.recap_loading);
        drop(st);
        assert!(!Recap.wants_redraw());
        assert!(!Recap.hooks().contains(&ExtensionHook::Dock));
    }

    #[test]
    fn command_and_mode_surface() {
        let cmds = Recap.commands();
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].name, "recap");
        assert!(cmds[0].busy_safe);
        let subs: Vec<&str> = cmds[0].subcommands.iter().map(|s| s.name).collect();
        assert_eq!(subs, vec!["now", "on", "off"]);
        assert_eq!(Recap.name(), EXT);
        assert!(!Recap.default_enabled(), "默认不启用");
        assert_eq!(
            Recap.modes(),
            vec![ExtensionMode::Dev, ExtensionMode::Creator],
            "只在 Dev / Creator 模式可用"
        );
        assert!(Recap.tools().is_empty());
    }

    #[test]
    fn session_switch_resets_recap_and_keeps_settings() {
        let _g = lock_global();
        reset_state();
        lock().recap = Some("stale".to_string());
        lock().auto_recap = false;
        Recap.on_session_switched(
            Some("/tmp/0193f1b2-1111-2222-3333-444455556666.jsonl"),
            &[message("user", "fresh start")],
        );
        let st = lock();
        assert!(st.recap.is_none(), "上一条会话的小结要清掉");
        assert!(!st.auto_recap, "设置不该被会话切换重置");
        assert_eq!(st.excerpt, "user: fresh start");
    }
}
