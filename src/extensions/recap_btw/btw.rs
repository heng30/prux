//! `btw` 扩展：只读旁问（`/btw <问题>`）。
//!
//! 顺手问一句，答案只出现在输入区位置的 `BTW` 面板里，Esc 关闭、空回车也关闭。
//! 它**不进入对话**——不改会话历史、不影响主模型接下来的判断。
//!
//! 面板自带一行 `btw>` 输入框（打开即聚焦）：可以**连续追问**，已完成的轮次留在面板里、
//! 也作为后续追问的上下文送给模型。面板显示期间主页输入框隐藏（`OverlaySize::Panel`），
//! 避免"看得见却打不了字"。
//!
//! 送给模型的只有最近 4 条消息、4000 字以内的摘录，加上一段"只读、只依据摘录、
//! 别装作用工具"的系统提示，`max_tokens` 限 500（关不掉思考的模型会抬到 `shared` 的下限）；
//! 请求本身是一次不带工具的一次性补全
//! （[`provider::simple_completion`]，见 [`super::shared::spawn_one_shot`]），
//! 所以也不会有任何工具副作用。
//!
//! **默认不启用**，且只在 `Dev` / `Creator` 扩展模式下可用。

use super::shared;
use crate::{
    core::{
        extensions::{
            CommandHost, DockSpan, Extension, ExtensionCommand, ExtensionHook, ExtensionMode,
            ExtensionTool, ExtensionUiRequest, OverlayEvent, OverlayInput, OverlayKey, OverlaySize,
            OverlayView, UiNotifyLevel, next_ui_id, request_ui,
        },
        provider::AgentMessage,
        session_manager::Session,
    },
    extensions::{
        ACCENT, ERROR, EXTENSION_FACTORIES, ExtensionFactory, MUTED, PRIORITY_BTW, command_arg,
        util::{session_id_of, spinner_frame},
    },
    modes::interactive::handlers::register_host_command,
    utils::{display::wrap_text, glyphs::DEF_FAILED},
};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

/// 扩展名。
const EXT: &str = "btw";

/// 系统提示：只读、只依据摘录、不假装执行任何操作。
const SYSTEM: &str = "You answer a quick side question about the coding session shown below. You are read-only: you \
     cannot run commands, edit files, or continue the task. Answer from the excerpt; if it does not \
     contain the answer, say so briefly. Keep it short and concrete.";

/// 摘录的消息条数上限。
const EXCERPT_MESSAGES: usize = 4;

/// 摘录的字符上限。
const EXCERPT_CHARS: usize = 4000;

/// 回复的 token 上限。
const MAX_TOKENS: u32 = 500;

/// 面板最多显示的行数。
const MAX_ROWS: u16 = 16;

/// 拼进提示的历史轮次上限（面板仍显示全部，只有送给模型的上下文受限）。
const PROMPT_HISTORY_TURNS: usize = 6;

/// 自声明工厂：linkme 分布式切片。
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static BTW_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_BTW,
    make: || -> Arc<dyn Extension> { Arc::new(Btw) },
};

/// 扩展状态（进程级单例）。
#[derive(Default)]
struct State {
    /// 面板的覆盖层 id（None = 未显示）。
    overlay_id: Option<u64>,
    /// 当前提问。
    question: String,
    /// 当前答案（None = 还在生成）。
    answer: Option<String>,
    /// 失败原因（None = 无错误）。
    error: Option<String>,
    /// 异步结果刚落地、还没渲染成帧（true = 下一帧必须重绘）。
    ///
    /// 空闲时 TUI 只在 80ms tick 里轮询 [`Extension::wants_redraw`]：等答案期间它一直为真，
    /// 结果一落地若直接变假，那一帧就永远不会被画出来（要等鼠标/按键事件才刷新）。
    refresh_pending: bool,
    /// 打开面板时的 provider（追问复用；None = 面板未打开）。
    provider: Option<String>,
    /// 打开面板时的模型 id（追问复用）。
    model_id: Option<String>,
    /// 打开面板时的会话摘录（追问复用）。
    excerpt: String,
    /// 已完成的旁问轮次 `(问题, 答案)`（旧→新），作为后续追问的上下文。
    history: Vec<(String, String)>,
    /// 在飞请求的代际：只有最新一代的结果会被采纳（关面板 / 换问题都会 +1）。
    request_gen: u64,
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

/// 清空面板状态（关闭 / 会话切换 / 禁用时调用）。
///
/// `request_gen` 一并 +1：在飞的回答回来时发现代际不符，直接丢弃，不会复活面板。
fn clear_state(st: &mut State) {
    st.overlay_id = None;
    st.question.clear();
    st.answer = None;
    st.error = None;
    st.refresh_pending = false;
    st.provider = None;
    st.model_id = None;
    st.excerpt.clear();
    st.history.clear();
    st.request_gen = st.request_gen.wrapping_add(1);
}

/// `btw` 扩展。
pub struct Btw;

impl Extension for Btw {
    /// 扩展名 `btw`。
    fn name(&self) -> &str {
        EXT
    }

    /// 一句话说明：只读旁问。
    fn description(&self) -> &str {
        "Ask a read-only side question (/btw <question>); the answer is never added to the conversation"
    }

    /// 默认**不启用**：虽不花钱就没有动作，但它属于 `Dev` / `Creator` 模式下的排查/探索工具，
    /// 由用户在 `/extension` 面板显式开启。
    fn default_enabled(&self) -> bool {
        false
    }

    /// 只在 `Dev` / `Creator` 扩展模式下可用。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Dev, ExtensionMode::Creator]
    }

    /// 不提供模型工具（旁问由用户从输入框发起）。
    fn tools(&self) -> Vec<ExtensionTool> {
        Vec::new()
    }

    /// 声明 `/btw` 命令。
    fn commands(&self) -> Vec<ExtensionCommand> {
        vec![ExtensionCommand {
            name: "btw".to_string(),
            description:
                "/btw <question> (read-only side question; never added to the conversation)"
                    .to_string(),
            busy_safe: true, // 只起一次性请求，不锁 agent
            subcommands: Vec::new(),
        }]
    }

    /// 注册 `/btw` 的 handler。
    fn on_registered(&self) {
        register_host_command(EXT, "btw", command_btw);
    }

    /// 记住当前会话 id（一次性请求会带上它做提示缓存路由）。
    fn on_session_start(&self, _cwd: &str, _messages: &[AgentMessage], session: Option<&Session>) {
        shared::remember_session(session.map(|s| s.session_id.clone()));
    }

    /// 会话切换：记住新会话 id，并收掉上一条会话留下的面板。
    fn on_session_switched(&self, session_path: Option<&str>, _messages: &[AgentMessage]) {
        shared::remember_session(session_id_of(session_path));
        let previous = {
            let mut st = lock();
            let previous = st.overlay_id;
            clear_state(&mut st);
            previous
        };
        if let Some(id) = previous {
            request_ui(ExtensionUiRequest::HideOverlay { id });
        }
    }

    /// 禁用时收起面板：禁用后框架不再轮询本扩展，`Closed` 回传也收不到。
    fn on_enabled_changed(&self, enabled: bool) {
        if !enabled {
            clear_state(&mut lock());
        }
    }

    /// 只在面板打开时订阅覆盖层事件（否则框架每帧空转）。
    fn hooks(&self) -> Vec<ExtensionHook> {
        if lock().overlay_id.is_some() {
            vec![ExtensionHook::Overlay]
        } else {
            Vec::new()
        }
    }

    /// 等答案期间保持重绘（面板要显示 spinner）；结果落地后再补一帧。
    fn wants_redraw(&self) -> bool {
        let st = lock();
        if st.overlay_id.is_none() {
            return false;
        }
        st.refresh_pending || (st.answer.is_none() && st.error.is_none())
    }

    /// 渲染 `BTW` 面板（历史轮次 + 当前问题 + 答案 / 生成中 / 错误 + `btw>` 输入行）。
    fn overlay_view(&self, id: u64, cols: u16) -> Option<OverlayView> {
        let mut st = lock();
        if st.overlay_id != Some(id) {
            return None;
        }

        // 内容即将被渲染：这一帧之后不再为此额外请求重绘
        st.refresh_pending = false;

        let width = usize::from(cols).max(20).saturating_sub(4);
        let mut lines: Vec<Vec<DockSpan>> = Vec::new();

        // 已完成的轮次在前（旧→新），当前一轮在最后
        for (question, answer) in &st.history {
            push_question(&mut lines, question, width);
            for line in wrap_text(answer, width) {
                lines.push(vec![DockSpan::plain(line)]);
            }
            lines.push(Vec::new());
        }

        push_question(&mut lines, &st.question, width);
        lines.push(Vec::new());

        if let Some(answer) = &st.answer {
            for line in wrap_text(answer, width) {
                lines.push(vec![DockSpan::plain(line)]);
            }
        } else if let Some(error) = &st.error {
            // 错误文案可能很长（`provider returned 400 Bad Request: {json}`），必须自己折行：
            // 渲染层 `dock_line_spans` 只按面板宽度截断，不折行，长错误会贴到右边缘被切掉。
            for (i, line) in wrap_text(error, width.saturating_sub(2))
                .into_iter()
                .enumerate()
            {
                let prefix = if i == 0 {
                    &format!("{DEF_FAILED} ")
                } else {
                    "  "
                };
                lines.push(vec![DockSpan::new(
                    ERROR.0,
                    ERROR.1,
                    format!("{prefix}{line}"),
                )]);
            }
        } else {
            lines.push(vec![DockSpan::new(
                ACCENT.0,
                ACCENT.1,
                format!("{} answering…", spinner_frame()),
            )]);
        }

        Some(OverlayView {
            title: "BTW".to_string(),
            header: Vec::new(),
            lines,
            messages: Vec::new(),
            footer: vec![
                ("Enter".to_string(), "ask".to_string()),
                ("Esc".to_string(), "close".to_string()),
            ],
            input: Some(OverlayInput {
                label: "btw>".to_string(),
                value: String::new(),
                placeholder: "ask a follow-up…".to_string(),
            }),
            editor: None,
            // Panel：占据输入区、隐藏主页输入框（Inline 会把主页输入框留在上面却打不了字）
            size: OverlaySize::Panel { max_rows: MAX_ROWS },
            selected: None,
            input_focus: true,
        })
    }

    /// 面板事件：空回车 / Esc 关面板，非空回车发起追问，关闭时清状态。
    fn on_overlay_event(&self, id: u64, ev: &OverlayEvent) -> bool {
        match ev {
            OverlayEvent::Closed => {
                let mut st = lock();
                if st.overlay_id == Some(id) {
                    clear_state(&mut st);
                    return true;
                }
                false
            }
            OverlayEvent::Submit { text } => {
                let text = text.trim().to_string();
                let current = {
                    let st = lock();
                    if st.overlay_id != Some(id) {
                        return false;
                    }
                    (st.question.clone(), st.answer.clone())
                };

                // 空回车 = 关面板（对齐“Esc / Enter 关闭”）
                if text.is_empty() {
                    close_panel(id);
                    return true;
                }

                // 完成的一轮进历史，作为后续追问的上下文
                let (question, answer) = current;
                if let Some(answer) = answer {
                    lock().history.push((question, answer));
                }
                ask(text);
                true
            }
            // 输入行聚焦时 TUI 的 Esc 只“退出输入态”、不会关面板；这里自己关，
            // 让“Esc 关闭”第一下就生效（否则输入行先消失、还要再按一下）。
            OverlayEvent::Key {
                key: OverlayKey::Esc,
                ..
            } => {
                if lock().overlay_id != Some(id) {
                    return false;
                }
                close_panel(id);
                true
            }
            _ => false,
        }
    }
}

/// 关闭面板：清状态并请求 TUI 隐藏。
fn close_panel(id: u64) {
    {
        let mut st = lock();
        if st.overlay_id == Some(id) {
            clear_state(&mut st);
        }
    }
    request_ui(ExtensionUiRequest::HideOverlay { id });
}

/// 往面板行里压一段问题：muted 色（与答案区分），不缩进（与答案左对齐）。
fn push_question(lines: &mut Vec<Vec<DockSpan>>, question: &str, width: usize) {
    for line in wrap_text(question, width) {
        lines.push(vec![DockSpan::new(MUTED.0, MUTED.1, line)]);
    }
}

/// `/btw` handler：打开面板并问第一个问题。
fn command_btw(host: &mut dyn CommandHost, raw: &str) -> bool {
    let question = command_arg(raw).trim().to_string();
    if question.is_empty() {
        host.push_message("Usage: /btw <question> (read-only side question; the answer is not added to the conversation)",
            UiNotifyLevel::Info,
        );
        return false;
    }

    let Some((provider, model_id)) = host.current_model() else {
        host.push_message(
            "No model is selected yet. Pick one with /model, then ask again.",
            UiNotifyLevel::Warning,
        );
        return false;
    };

    // 扩展若是本会话中途启用，`on_session_start` 没跑过、会话 id 槽是空的；
    // opencode 系 provider 缺 `x-opencode-session` 会直接 400，命令执行时补一次。
    if let Some(path) = host.session_path()
        && let Some(id) = session_id_of(Some(&path))
    {
        shared::remember_session(Some(id));
    }

    let excerpt = host.transcript_excerpt(EXCERPT_MESSAGES, EXCERPT_CHARS);

    let id = next_ui_id();
    {
        let mut state = lock();
        state.overlay_id = Some(id);
        state.provider = Some(provider);
        state.model_id = Some(model_id);
        state.excerpt = excerpt;
        state.history.clear();
        state.question.clear();
        state.answer = None;
        state.error = None;
    }
    request_ui(ExtensionUiRequest::ShowOverlay { id });
    ask(question);
    false
}

/// 发起一次旁问：把问题连同摘录、此前的旁问轮次拼成提示，结果落到面板。
///
/// 面板未打开（或没有型号快照）时返回 `false`，不发起请求。每次调用都会推进
/// `request_gen`，上一轮还在飞的回答回来时代际不符，会被丢弃。
fn ask(question: String) -> bool {
    let (provider, model_id, prompt) = {
        let st = lock();
        let (Some(provider), Some(model_id)) = (st.provider.clone(), st.model_id.clone()) else {
            return false;
        };
        (provider, model_id, build_prompt(&st, &question))
    };

    let generation = {
        let mut st = lock();
        st.question = question;
        st.answer = None;
        st.error = None;
        st.request_gen = st.request_gen.wrapping_add(1);
        st.request_gen
    };

    if let Err(e) = shared::spawn_one_shot(
        provider,
        model_id,
        SYSTEM,
        prompt,
        MAX_TOKENS,
        move |result| {
            let mut st = lock();
            // 面板已关 / 已经换了一轮问题：只在还等着这一代回答时回填
            if st.request_gen != generation {
                return;
            }
            match result {
                Ok(text) => st.answer = Some(text),
                Err(e) => st.error = Some(e),
            }
            st.refresh_pending = true;
        },
    ) {
        let mut st = lock();
        if st.request_gen == generation {
            st.error = Some(e);
            st.refresh_pending = true;
        }
    }
    true
}

/// 拼一次性请求的用户提示：会话摘录 + 此前旁问轮次 + 本次问题。
fn build_prompt(st: &State, question: &str) -> String {
    let mut prompt = String::new();
    if !st.excerpt.trim().is_empty() {
        prompt.push_str("Session excerpt:\n");
        prompt.push_str(&st.excerpt);
        prompt.push_str("\n\n");
    }

    if !st.history.is_empty() {
        prompt.push_str("Earlier side questions in this panel:\n");
        let start = st.history.len().saturating_sub(PROMPT_HISTORY_TURNS);
        for (question, answer) in &st.history[start..] {
            prompt.push_str("Q: ");
            prompt.push_str(question);
            prompt.push_str("\nA: ");
            prompt.push_str(answer);
            prompt.push('\n');
        }
        prompt.push('\n');
    }
    prompt.push_str("Question: ");
    prompt.push_str(question);
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modes::interactive::app::App;
    use crate::test_support::AUTH_TEST_LOCK;

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

    /// 复位扩展状态。
    fn reset_state() {
        *lock() = State::default();
    }

    /// 面板行拼成一段文本。
    fn panel_text(view: &OverlayView) -> String {
        view.lines
            .iter()
            .map(|line| line.iter().map(|s| s.text.clone()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn btw_without_question_prints_usage() {
        let _g = lock_global();
        reset_state();
        let mut st = App::new();
        assert!(!command_btw(&mut st, "btw"));
        assert!(!st.system_messages.is_empty(), "无参应提示用法");
        assert_eq!(lock().overlay_id, None, "不该打开面板");
    }

    #[test]
    fn btw_without_model_warns() {
        let _g = lock_global();
        reset_state();
        let mut st = App::new();
        st.current_provider = None;
        st.current_model = None;
        assert!(!command_btw(&mut st, "btw what is this?"));
        assert!(!st.system_messages.is_empty(), "没有型号应提示");
        assert_eq!(lock().overlay_id, None);
    }

    #[test]
    fn long_error_is_wrapped_instead_of_truncated() {
        let _g = lock_global();
        reset_state();
        let id = next_ui_id();
        {
            let mut st = lock();
            st.overlay_id = Some(id);
            st.question = "q".to_string();
            st.error = Some("provider returned 400 Bad Request: ".to_string() + &"x".repeat(200));
        }

        let ext = Btw;
        let text = panel_text(&ext.overlay_view(id, 40).unwrap());
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines.len() > 3, "长错误必须折成多行，而不是一行被截断");
        assert!(
            lines.iter().all(|l| l.chars().count() <= 40),
            "每行都不该超出面板宽度：{lines:?}"
        );
        assert!(lines.iter().any(|l| l.contains('✗')), "首行带错误标记");
    }

    #[test]
    fn btw_recovers_session_id_from_app_path() {
        let _g = lock_global();
        reset_state();
        // 模拟“扩展本会话中途启用”：会话钩子没跑过，槽是空的
        shared::remember_session(None);
        let mut st = App::new();
        st.current_provider = Some("opencode".to_string());
        st.current_model = Some("kimi-k2.5".to_string());
        st.current_session_path =
            Some("/tmp/0193f1b2-1111-2222-3333-444455556666.jsonl".to_string());

        assert!(!command_btw(&mut st, "btw hi"));
        assert_eq!(
            shared::remembered_session().as_deref(),
            Some("0193f1b2-1111-2222-3333-444455556666"),
            "从 App 的会话路径回填 id，opencode 才有 x-opencode-session"
        );
    }

    #[test]
    fn answer_landing_requests_one_more_frame() {
        let _g = lock_global();
        reset_state();
        let id = next_ui_id();
        {
            let mut st = lock();
            st.overlay_id = Some(id);
            st.answer = Some("done".to_string());
            st.refresh_pending = true;
        }

        let ext = Btw;
        assert!(ext.wants_redraw(), "结果刚落地的下一帧必须重绘");
        let _ = ext.overlay_view(id, 80).unwrap();
        assert!(!ext.wants_redraw(), "渲染过一帧后不再持续重绘");
    }

    #[test]
    fn btw_opens_panel_and_renders_error_without_runtime() {
        let _g = lock_global();
        reset_state();
        let mut st = App::new();
        st.current_provider = Some("anthropic".to_string());
        st.current_model = Some("claude-fable-5".to_string());
        st.messages.push(message("user", "hello world"));

        assert!(!command_btw(&mut st, "btw what did I ask?"));
        let id = lock().overlay_id.expect("应打开面板");
        assert!(lock().error.is_some(), "单测没有 tokio 运行时应记录错误");

        let ext = Btw;
        let view = ext.overlay_view(id, 80).expect("认领自己的 id");
        assert_eq!(view.title, "BTW");
        assert!(
            matches!(view.size, OverlaySize::Panel { .. }),
            "面板要占据输入区、隐藏主页输入框"
        );
        assert!(view.input.is_some(), "面板要自带追问输入行");
        assert!(view.input_focus, "打开即聚焦输入行");
        assert!(ext.overlay_view(id + 1, 80).is_none());
        let text = panel_text(&view);
        assert!(text.contains("what did I ask?"), "面板要显示问题");
        assert!(text.contains('✗'), "面板要显示错误");

        // 答案就绪后不再要求重绘（面板是静态的）
        lock().error = None;
        lock().answer = Some("you asked about the thing".to_string());
        assert!(!ext.wants_redraw());
        assert!(
            panel_text(&ext.overlay_view(id, 80).unwrap()).contains("you asked about the thing")
        );

        assert!(ext.on_overlay_event(id, &OverlayEvent::Closed));
        assert_eq!(lock().overlay_id, None);
        assert!(!ext.on_overlay_event(id, &OverlayEvent::Closed));
    }

    #[test]
    fn late_answer_is_dropped_when_panel_closed() {
        let _g = lock_global();
        reset_state();
        let id = next_ui_id();
        {
            let mut st = lock();
            st.overlay_id = Some(id);
            st.question = "q".to_string();
        }
        // 面板被关掉（Esc）后，迟到的回答不该复活面板状态
        Btw.on_overlay_event(id, &OverlayEvent::Closed);
        let st = lock();
        assert_eq!(st.overlay_id, None);
        assert!(st.answer.is_none());
    }

    #[test]
    fn empty_submit_closes_and_text_submit_starts_a_new_turn() {
        let _g = lock_global();
        reset_state();
        while crate::core::extensions::take_pending_ui().is_some() {}

        let id = next_ui_id();
        {
            let mut st = lock();
            st.overlay_id = Some(id);
            st.provider = Some("anthropic".to_string());
            st.model_id = Some("claude-fable-5".to_string());
            st.excerpt = "user: hi".to_string();
            st.question = "first?".to_string();
            st.answer = Some("first answer".to_string());
        }

        // 非空回车：上一轮进历史，当前问题换成新的
        assert!(Btw.on_overlay_event(
            id,
            &OverlayEvent::Submit {
                text: "  follow up  ".to_string()
            }
        ));
        {
            let st = lock();
            assert_eq!(
                st.history,
                vec![("first?".to_string(), "first answer".to_string())],
                "完成的一轮要进历史"
            );
            assert_eq!(st.question, "follow up", "新问题要 trim 后成为当前问题");
            assert!(st.answer.is_none(), "新一轮还没有答案");
            assert!(st.error.is_some(), "单测没有 tokio 运行时 → 记录错误");
        }

        // 空回车：关面板并请求 TUI 隐藏
        assert!(Btw.on_overlay_event(
            id,
            &OverlayEvent::Submit {
                text: String::new()
            }
        ));
        assert_eq!(lock().overlay_id, None, "空回车应关面板");
        assert!(matches!(
            crate::core::extensions::take_pending_ui(),
            Some(ExtensionUiRequest::HideOverlay { id: hid }) if hid == id
        ));
    }

    #[test]
    fn escape_closes_the_panel_on_the_first_press() {
        let _g = lock_global();
        reset_state();
        while crate::core::extensions::take_pending_ui().is_some() {}

        let id = next_ui_id();
        lock().overlay_id = Some(id);
        assert!(Btw.on_overlay_event(
            id,
            &OverlayEvent::Key {
                key: OverlayKey::Esc,
                input: Some(String::new()),
            }
        ));
        assert_eq!(lock().overlay_id, None, "Esc 应关面板");
        assert!(matches!(
            crate::core::extensions::take_pending_ui(),
            Some(ExtensionUiRequest::HideOverlay { id: hid }) if hid == id
        ));
    }

    #[test]
    fn question_and_answer_lines_are_left_aligned() {
        let _g = lock_global();
        reset_state();
        let id = next_ui_id();
        {
            let mut st = lock();
            st.overlay_id = Some(id);
            st.question = "question".to_string();
            st.answer = Some("answer".to_string());
        }

        let text = panel_text(&Btw.overlay_view(id, 40).unwrap());
        let question = text.lines().find(|l| l.contains("question")).unwrap();
        let answer = text.lines().find(|l| l.contains("answer")).unwrap();
        assert_eq!(question, "question", "问题不应带左侧 padding");
        assert_eq!(answer, "answer", "答案不带左侧 padding");
    }

    #[test]
    fn follow_up_prompt_carries_excerpt_and_history() {
        let _g = lock_global();
        reset_state();
        let st = State {
            excerpt: "user: do the thing".to_string(),
            history: vec![("what now?".to_string(), "keep going".to_string())],
            ..State::default()
        };

        let prompt = build_prompt(&st, "and then?");
        assert!(prompt.contains("user: do the thing"), "带会话摘录");
        assert!(prompt.contains("Q: what now?"), "带历史问题");
        assert!(prompt.contains("A: keep going"), "带历史答案");
        assert!(
            prompt.trim_end().ends_with("Question: and then?"),
            "末行是本次问题"
        );

        let bare = build_prompt(&State::default(), "hi");
        assert_eq!(bare, "Question: hi", "没有摘录/历史时只有问题");
    }

    #[test]
    fn disabled_extension_clears_state_and_hooks() {
        let _g = lock_global();
        reset_state();
        let id = next_ui_id();
        {
            let mut st = lock();
            st.overlay_id = Some(id);
            st.question = "q".to_string();
        }
        assert_eq!(Btw.hooks(), vec![ExtensionHook::Overlay]);
        assert!(Btw.wants_redraw());

        Btw.on_enabled_changed(false);
        assert_eq!(lock().overlay_id, None);
        assert!(!Btw.wants_redraw());
        assert!(Btw.hooks().is_empty());
    }

    #[test]
    fn command_and_mode_surface() {
        let cmds = Btw.commands();
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].name, "btw");
        assert!(cmds[0].busy_safe);
        assert!(cmds[0].subcommands.is_empty());
        assert_eq!(Btw.name(), EXT);
        assert!(!Btw.default_enabled(), "默认不启用");
        assert_eq!(
            Btw.modes(),
            vec![ExtensionMode::Dev, ExtensionMode::Creator],
            "只在 Dev / Creator 模式可用"
        );
        assert!(Btw.tools().is_empty());
    }

    #[test]
    fn session_switch_remembers_id_and_closes_panel() {
        let _g = lock_global();
        reset_state();
        let id = next_ui_id();
        lock().overlay_id = Some(id);
        while crate::core::extensions::take_pending_ui().is_some() {}

        Btw.on_session_switched(Some("/tmp/0193f1b2-1111-2222-3333-444455556666.jsonl"), &[]);
        assert_eq!(lock().overlay_id, None, "会话换了就收掉面板");
        assert!(matches!(
            crate::core::extensions::take_pending_ui(),
            Some(ExtensionUiRequest::HideOverlay { id: hid }) if hid == id
        ));
    }
}
