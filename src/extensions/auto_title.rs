//! `auto-title` 扩展：会话首轮结束后用一次短请求给会话起名。
//!
//! 会话名是 `name` fact（`/name` 写的同一个东西），侧栏与状态栏都读它。本扩展在**首轮
//! 结束时**取「首条用户输入 + assistant 的首段正文」起一次一次性请求（[`shared::spawn_one_shot`]），
//! 把结果规整成一行后写回会话名：
//!
//! - 只在**主 agent** 的首轮触发，子代理与后续轮次一律不参与；
//! - 恢复的旧会话（已有消息）与已经有名字的会话**不碰**——用户 `/name` 改过的名字永远优先；
//! - 失败时退回用户输入的前几个词，不打扰用户（只花一次请求，不留下错误噪音）；
//! - `/title` 供用户强制用模型重命名（无参数）：不限轮次、不看旧名字，旧会话也能补名字；
//! - `/title <名字>` 是**手动**起名：一个模型请求都不发，直接把这段文字写成会话名；
//! - `/title set --provider <p> --model <m>` 把**命名专用模型**写进
//!   `agent_dir()/extensions/auto-title.json`（经 [`ConfigCache`] 读写），之后自动命名与
//!   `/title` 都用它——起名这种一句话的活儿不必动用当前会话那个贵的模型；
//!   `/title unset` 清掉配置，回到"用当前会话模型"。
//!
//! **默认不启用**（会改动会话显示名，属于"抢行为"，按仓库约定由用户在 `/extension` 里开启）。

use super::{
    EXTENSION_FACTORIES, ExtensionFactory, PRIORITY_AUTO_TITLE,
    recap_btw::shared,
    util::{check_physical, notify_text, session_id_of, split_model},
};
use crate::{
    core::{
        config_cache::ConfigCache,
        extensions::{
            BoundaryOutcome, CommandHost, Extension, ExtensionCommand, ExtensionHook,
            ExtensionMode, ExtensionTool, ExtensionUiRequest, SubcommandDef, ToolExecCtx,
            UiNotifyLevel, request_ui,
        },
        provider::{AgentMessage, ModelType},
        session_manager::Session,
        settings_manager::agent_dir,
    },
    extensions::command_arg,
    modes::tui::handlers::register_host_command,
    utils::{display::flatten_line, truncate::truncate_chars_within},
};
use serde_json::Value;
use std::{
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard, OnceLock, RwLock},
};

/// 扩展名（也是配置文件前缀）。
const EXT: &str = "auto-title";

/// 斜杠命令名（`/title`：短一点好敲，扩展身份仍是 [`EXT`]）。
const CMD: &str = "title";

/// 系统提示：一行短标题、语言跟首条用户输入走。
const SYSTEM: &str = "Name this coding session with a title of at most 20 characters, \
                      in the language of the user's first message. Say what is being worked \
                      on, not what was asked. Never put file paths, `@` references or URLs \
                      in the title. Answer with the title alone: no quotes, \
                      no 'Title:' prefix, no trailing period.";

/// 回复的 token 上限（标题很短；关不掉思考的模型由 `shared` 抬到下限）。
const MAX_TOKENS: u32 = 48;

/// 会话名的字符上限（超出按字符硬截断，省略号占配额）。
const MAX_CHARS: usize = 20;

/// 命名素材里用户输入的字符上限。
const PROMPT_CHARS: usize = 600;

/// 命名素材里 assistant 回复的字符上限。
const REPLY_CHARS: usize = 600;

/// `/title` 取用的转录条数。
const EXCERPT_MESSAGES: usize = 12;

/// `/title` 取用的转录字符上限。
const EXCERPT_CHARS: usize = 12000;

/// 配置文件相对 agent 目录的路径。
const CONFIG_REL: &str = "extensions/auto-title.json";

/// 配置里命名模型所属 provider 的键。
const CONFIG_PROVIDER: &str = "provider";

/// 配置里命名模型 id 的键。
const CONFIG_MODEL: &str = "model";

/// 子命令：配置命名专用模型。
const SUB_SET: &str = "set";

/// 子命令：清除命名专用模型。
const SUB_UNSET: &str = "unset";

/// 用法提示（`set` 缺参时给出）。
const USAGE: &str = "usage: /title (name from the transcript) | \
                     /title <name> (set that name directly) | \
                     /title set --provider <provider> --model <model> | \
                     /title unset";

/// 自声明工厂：linkme 分布式切片。
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static AUTO_TITLE_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_AUTO_TITLE,
    make: || -> Arc<dyn Extension> { Arc::new(AutoTitle) },
};

/// 配置文件的读写锁（面板/命令写盘与按需读盘互斥）。
static CONFIG_LOCK: RwLock<()> = RwLock::new(());

/// 配置文件的读缓存（锁序恒为「RwLock → 缓存内层 Mutex」）。
static CONFIG_CACHE: ConfigCache = ConfigCache::new();

/// 自动命名状态（进程级单例；会话建立 / 切换时按会话重置，型号跨会话保留）。
#[derive(Default)]
struct State {
    /// 本会话不参与自动命名：已有名字，或是带消息恢复的旧会话。
    skip: bool,
    /// 本会话已发起过命名（含失败），不再重试。
    done: bool,
    /// 首条用户输入（命名素材；非空即"素材已收下"）。
    prompt: Option<String>,
    /// 当前生效的型号 `(provider, model_id)`，由 `on_exec_ctx` 缓存。
    model: Option<(String, String)>,
    /// 已绑定的会话 id：`None` = 还没收到会话通知，`Some(None)` = 刚 `/new` 且文件未落盘，
    /// `Some(Some(id))` = 已绑定到该会话。
    session: Option<Option<String>>,
}

/// 取状态槽（进程级单例）。
fn state() -> &'static Mutex<State> {
    /// 自动命名状态槽。
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(State::default()))
}

/// 取状态锁；锁被毒化时取回内部值。
fn lock() -> MutexGuard<'static, State> {
    state().lock().unwrap_or_else(|e| e.into_inner())
}

/// 按会话重置（型号保留：它与具体会话无关）。
fn reset(skip: bool) {
    let mut st = lock();
    st.skip = skip;
    st.done = false;
    st.prompt = None;
}

/// 配置文件路径：`agent_dir()/extensions/auto-title.json`。
fn config_path() -> PathBuf {
    agent_dir().join(CONFIG_REL)
}

/// 配置里的命名专用模型：未配置、缺键、空值或文件不可读时 `None`（回落到当前会话模型）。
fn config_model() -> Option<(String, String)> {
    let path = config_path();
    let root = CONFIG_CACHE.read_object(&CONFIG_LOCK, &path)?;
    let read = |key: &str| {
        root.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    Some((read(CONFIG_PROVIDER)?, read(CONFIG_MODEL)?))
}

/// 写命名专用模型：`None` = 清掉这两个键（回到"用当前会话模型"）；其它键原样保留。
fn store_model(model: Option<(&str, &str)>) -> std::io::Result<()> {
    let path = config_path();
    let mut root = CONFIG_CACHE
        .read_object(&CONFIG_LOCK, &path)
        .unwrap_or_else(|| serde_json::json!({}));
    if let Some(map) = root.as_object_mut() {
        match model {
            Some((provider, id)) => {
                map.insert(
                    CONFIG_PROVIDER.to_string(),
                    Value::String(provider.to_string()),
                );
                map.insert(CONFIG_MODEL.to_string(), Value::String(id.to_string()));
            }
            None => {
                map.remove(CONFIG_PROVIDER);
                map.remove(CONFIG_MODEL);
            }
        }
    }
    CONFIG_CACHE.write_object(&CONFIG_LOCK, &path, root)
}

/// 命名实际使用的模型：配置优先，没有配置就用 `on_exec_ctx` 缓存下来的当前会话模型。
///
/// 不得在持状态锁时调用（它会读配置并再取一次状态锁）。
fn naming_model() -> Option<(String, String)> {
    config_model().or_else(|| lock().model.clone())
}

/// `auto-title` 扩展。
pub struct AutoTitle;

impl Extension for AutoTitle {
    /// 扩展名 `auto-title`。
    fn name(&self) -> &str {
        EXT
    }

    /// 一句话说明：首轮结束后自动给会话起名。
    fn description(&self) -> &str {
        "Auto-name the session after its first turn (/title to rename)"
    }

    /// 仅 `Dev` / `Creator` 模式可用：它会自己写配置文件。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Dev, ExtensionMode::Creator]
    }

    /// 默认**不启用**：它会改动会话显示名（侧栏 / 状态栏），属于抢行为，交给用户开启。
    fn default_enabled(&self) -> bool {
        false
    }

    /// 不提供模型工具（命名是扩展自己的事）。
    fn tools(&self) -> Vec<ExtensionTool> {
        Vec::new()
    }

    /// 声明 `/title`：显式给当前会话起名（旧会话补名字也走它）。
    fn commands(&self) -> Vec<ExtensionCommand> {
        vec![ExtensionCommand {
            name: CMD.to_string(),
            description: "/title [<name>] (model-named, or set <name> directly)".to_string(),
            busy_safe: true, // 只起一次性请求 / 写配置文件，不锁 agent
            subcommands: vec![
                SubcommandDef {
                    name: SUB_SET,
                    description: "Name with a specific `--provider <provider> --model <model>` (saved to the config file)",
                },
                SubcommandDef {
                    name: SUB_UNSET,
                    description: "Drop that model and name with the session's own model again",
                },
            ],
        }]
    }

    /// 关心轮收尾（触发命名）与用户提交（收素材）。
    fn hooks(&self) -> Vec<ExtensionHook> {
        vec![ExtensionHook::Boundary, ExtensionHook::UserPrompt]
    }

    /// 注册 `/title` 的 handler。
    fn on_registered(&self) {
        register_host_command(EXT, CMD, command_title);
    }

    /// 会话建立：全新会话才参与自动命名；已有消息或已有名字的会话跳过。
    fn on_session_start(&self, _cwd: &str, messages: &[AgentMessage], session: Option<&Session>) {
        shared::remember_session(session.map(|s| s.session_id.clone()));
        let skip = !messages.is_empty() || session.is_some_and(|s| s.name.is_some());
        reset(skip);
        lock().session = Some(session.map(|s| s.session_id.clone()));
    }

    /// 会话切换：同上；空会话也可能已被 `/name` 起过名，读一次文件确认。
    ///
    /// `/new` 的新会话是延迟落盘的，文件建出来时 UI 会**再发一次**同样的通知（带的是当前
    /// 转录，而非重新加载的历史）——那不是切换，见下面对 `reannounced` 的判定。
    fn on_session_switched(&self, session_path: Option<&str>, messages: &[AgentMessage]) {
        let id = session_id_of(session_path);
        shared::remember_session(id.clone());

        // shortcut: 只在"刚 /new、还没落盘、本会话已经收下素材"时把携带路径的通知认作本会话
        // 自己的文件；此时若用户其实切走去了另一个会话，状态会留在本会话（素材已是旧文本）。
        // 该窗口极短（文件在首条消息落盘时就建），故不额外区分。
        let reannounced = {
            let mut st = lock();
            let reannounced =
                st.session == Some(None) && id.is_some() && (st.prompt.is_some() || st.done);
            st.session = Some(id);
            reannounced
        };
        if reannounced {
            return;
        }

        let named = messages.is_empty()
            && session_path
                .and_then(|p| Session::open(p).ok())
                .is_some_and(|s| s.name.is_some());
        reset(named || !messages.is_empty());
    }

    /// 会话 id 已知（`/new` 尚未落盘也有）：命名请求带上它，缺了 opencode 会回 400。
    fn on_session_id(&self, session_id: &str) {
        shared::remember_session(Some(session_id.to_string()));
    }

    /// 缓存当前型号（命名请求要用；子代理上下文不覆盖）。
    fn on_exec_ctx(&self, ctx: &ToolExecCtx) {
        if ctx.depth > 0 {
            return;
        }
        if let Some(model) = ctx.parent_model.as_deref().and_then(split_model) {
            lock().model = Some(model);
        }
    }

    /// 收首条用户输入作为命名素材（后续提交与 steer 不再覆盖）。
    fn on_user_submit(&self, text: &str, _busy: bool) {
        let mut st = lock();
        if st.prompt.is_some() {
            return;
        }
        let prompt = flatten_line(text);
        if !prompt.is_empty() {
            st.prompt = Some(truncate_chars_within(&prompt, PROMPT_CHARS, "…"));
        }
    }

    /// 首轮结束（`turn_end`）时发起命名：主 agent、未被跳过、还没起过名。
    fn on_boundary(&self, event: &Value) -> Option<BoundaryOutcome> {
        if event.get("type").and_then(Value::as_str) != Some("turn_end")
            || event.get("agentScope").and_then(Value::as_str) != Some("main")
        {
            return None;
        }
        // 中断 / 出错的轮次不拿来起名（素材是半截的或干脆是错误文案），留给下一轮。
        if event.get("outcome").and_then(Value::as_str) != Some("completed") {
            return None;
        }
        let reply = event
            .get("message")
            .cloned()
            .and_then(|m| serde_json::from_value::<AgentMessage>(m).ok())
            .map(|m| flatten_line(&m.text()))
            .unwrap_or_default();

        let prompt = {
            let mut st = lock();
            if st.skip || st.done {
                return None;
            }
            let prompt = st.prompt.clone()?;
            if reply.is_empty() {
                return None;
            }
            st.done = true;
            prompt
        };

        if let Some((provider, model_id)) = naming_model() {
            spawn_title(
                provider,
                model_id,
                naming_prompt(&prompt, &reply),
                &prompt,
                false,
            );
        }
        None
    }
}

/// `/title` handler：`set` / `unset` 管配置，带别的文字是手动起名，空参数才调模型起名。
fn command_title(host: &mut dyn CommandHost, raw: &str) -> bool {
    let arg = command_arg(raw);
    let (sub, rest) = match arg.split_once(char::is_whitespace) {
        Some((sub, rest)) => (sub, rest.trim()),
        None => (arg, ""),
    };
    match sub {
        SUB_SET => return command_title_set(host, rest),
        SUB_UNSET => return command_title_unset(host),
        // 空参数 = 强制让模型起名（下面的路径）。
        "" => {}
        // 带了别的词 = 手动起名：不调模型，直接拿这段文字当会话名（与 `/name` 同效）。
        _ => return command_title_manual(arg),
    }

    // 本会话中途启用时会话钩子没跑过，槽里没有 id（opencode 会报缺 x-opencode-session）
    if let Some(path) = host.session_path()
        && let Some(id) = session_id_of(Some(&path))
    {
        shared::remember_session(Some(id));
    }

    // 配了命名专用模型就用它（题面就图省一次贵的请求），没配才用当前会话模型。
    let Some((provider, model_id)) = config_model().or_else(|| host.current_model()) else {
        host.push_message(
            "No model is selected yet. Pick one with /model, then try again.",
            UiNotifyLevel::Warning,
        );
        return false;
    };

    let excerpt = host.transcript_excerpt(EXCERPT_MESSAGES, EXCERPT_CHARS);
    if excerpt.trim().is_empty() {
        host.push_message(
            "Nothing to name yet — this session has no messages.",
            UiNotifyLevel::Info,
        );
        return false;
    }

    host.set_status("naming session…");
    let fallback = flatten_line(&excerpt);
    spawn_title(
        provider,
        model_id,
        naming_prompt("", &excerpt),
        &fallback,
        true,
    );
    false
}

/// `/title <名字>` handler：手动起名，不调模型，直接把参数写成会话名。
///
/// 名称压成一行并截到 [`MAX_CHARS`]；写入与自动命名走同一条 [`ExtensionUiRequest::SetSessionName`]。
fn command_title_manual(text: &str) -> bool {
    let name = truncate_chars_within(&flatten_line(text), MAX_CHARS, "…");
    request_ui(ExtensionUiRequest::SetSessionName { name });
    false
}

/// `/title set --provider <p> --model <m>` handler：配置命名专用模型。
///
/// 两个 flag 缺一不发（`set` 不带参数只报用法）：清除是 `/title unset` 的事，
/// 免得手滑一条 `set` 就把配好的模型抹了。
fn command_title_set(host: &mut dyn CommandHost, rest: &str) -> bool {
    let mut provider: Option<String> = None;
    let mut model: Option<String> = None;
    let mut tokens = rest.split_whitespace();

    while let Some(token) = tokens.next() {
        let (flag, inline) = match token.split_once('=') {
            Some((flag, value)) => (flag, Some(value.to_string())),
            None => (token, None),
        };
        let slot = match flag {
            "--provider" => &mut provider,
            "--model" => &mut model,
            other => {
                host.push_message(
                    &format!("auto-title: unknown flag `{other}` (use --provider / --model)"),
                    UiNotifyLevel::Warning,
                );
                return false;
            }
        };
        match inline.or_else(|| tokens.next().map(str::to_string)) {
            Some(value) if !value.trim().is_empty() => *slot = Some(value.trim().to_string()),
            _ => {
                host.push_message(
                    &format!("auto-title: {flag} requires a value"),
                    UiNotifyLevel::Warning,
                );
                return false;
            }
        }
    }

    let saved = match (provider, model) {
        (None, None) => {
            host.push_message(USAGE, UiNotifyLevel::Warning);
            return false;
        }
        (Some(provider), Some(model)) => {
            // 只认目录里的物理 chat 模型：配错了在起名时才炸，等于把错误拖到看不见的地方。
            if let Err(e) = check_physical(
                &provider,
                &model,
                ModelType::Chat,
                "title model",
                (EXT, EXT),
            ) {
                host.push_message(&format!("auto-title: {e}"), UiNotifyLevel::Warning);
                return false;
            }
            store_model(Some((&provider, &model)))
                .map(|()| format!("auto-title: naming with {provider}/{model} from now on"))
        }
        _ => {
            host.push_message(
                &format!("auto-title: set needs both --provider and --model — {USAGE}"),
                UiNotifyLevel::Warning,
            );
            return false;
        }
    };

    match saved {
        Ok(msg) => host.push_message(&msg, UiNotifyLevel::Info),
        Err(e) => host.push_message(
            &format!("auto-title: could not save {}: {e}", CONFIG_REL),
            UiNotifyLevel::Warning,
        ),
    }
    false
}

/// `/title unset` handler：清掉命名专用模型，回到"用当前会话模型"。
fn command_title_unset(host: &mut dyn CommandHost) -> bool {
    match store_model(None) {
        Ok(()) => host.push_message(
            "auto-title: naming with the current session model again",
            UiNotifyLevel::Info,
        ),
        Err(e) => host.push_message(
            &format!("auto-title: could not save {}: {e}", CONFIG_REL),
            UiNotifyLevel::Warning,
        ),
    }
    false
}

/// 起一次命名请求：结果规整成会话名写回，失败退回 `fallback` 的前几个词。
///
/// `notify` 决定失败时是否往聊天区报一条警告（`/title` 报，自动命名静默）。
fn spawn_title(provider: String, model_id: String, prompt: String, fallback: &str, notify: bool) {
    let fallback = fallback_title(fallback);
    let done = move |result: Result<String, String>| {
        let title = match result {
            Ok(text) => {
                let title = normalize_title(&text);
                if title.is_empty() { fallback } else { title }
            }
            Err(e) => {
                if notify {
                    notify_text(&format!("auto-title failed: {e}"), UiNotifyLevel::Warning);
                }
                fallback
            }
        };
        request_ui(ExtensionUiRequest::SetSessionName { name: title });
    };

    if let Err(e) = shared::spawn_one_shot(provider, model_id, SYSTEM, prompt, MAX_TOKENS, done)
        && notify
    {
        notify_text(&format!("auto-title failed: {e}"), UiNotifyLevel::Warning);
    }
}

/// 命名素材：首条用户输入 + assistant 首段正文（两者都截断，压成单行）。
fn naming_prompt(prompt: &str, reply: &str) -> String {
    let reply = truncate_chars_within(reply, REPLY_CHARS, "…");
    match prompt.is_empty() {
        true => format!("Session transcript so far:\n{reply}"),
        false => format!("First user message:\n{prompt}\n\nAssistant so far:\n{reply}"),
    }
}

/// 规整模型给出的标题：去前缀 / 引号 / 反引号 / 结尾标点，归一空白，按字符上限截断。
fn normalize_title(text: &str) -> String {
    let mut cleaned = flatten_line(text);
    for prefix in ["Title:", "title:", "Session:", "session:"] {
        if let Some(rest) = cleaned.strip_prefix(prefix) {
            cleaned = flatten_line(rest);
        }
    }
    // 头尾都要反复剥：模型常把标点写在引号里（`"Fix parser."`），一轮只去一层不够。
    let quoted = |c: char| matches!(c, '"' | '\'' | '`' | '“' | '”' | '「' | '」');
    let trailing = |c: char| matches!(c, '.' | '。' | ',' | '，' | ';' | '；' | ':');
    let cleaned = cleaned.trim_start_matches(quoted).trim();
    let cleaned = cleaned
        .trim_end_matches(|c: char| quoted(c) || trailing(c))
        .trim_end();
    truncate_chars_within(&drop_path_tokens(cleaned), MAX_CHARS, "…")
}

/// 该词是否像路径：`@` 文件引用、`./` / `../` / `~/` 相对路径、多段 `/` 路径、反斜杠路径。
///
/// 只认形状不看磁盘；单段 `/new` 这类斜杠命令不算（名字里带它是有意义的）。
fn is_path_token(token: &str) -> bool {
    token.starts_with('@')
        || token.starts_with("./")
        || token.starts_with("../")
        || token.starts_with("~/")
        || token.matches('/').count() >= 2
        || token.contains('\\')
}

/// 从一行里剔掉路径词：会话名只有 [`MAX_CHARS`] 的预算，路径一进来就把它吃光。
///
/// 全被剔空时原样返回，免得名字变成空串。
fn drop_path_tokens(line: &str) -> String {
    let kept: Vec<&str> = line
        .split_whitespace()
        .filter(|token| !is_path_token(token))
        .collect();
    if kept.is_empty() {
        line.to_string()
    } else {
        kept.join(" ")
    }
}

/// 兜底标题：素材的前几个词（剔掉路径，压成单行后按字符上限截断）。
fn fallback_title(source: &str) -> String {
    let line = drop_path_tokens(&flatten_line(source));
    if line.is_empty() {
        return EXT.to_string();
    }
    truncate_chars_within(&line, MAX_CHARS, "…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::extensions::take_pending_ui;

    /// 全局状态锁（进程级状态槽 + UI 请求队列）。
    fn lock_global() -> MutexGuard<'static, ()> {
        crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// 一条指定角色的消息。
    fn message(role: &str, text: &str) -> AgentMessage {
        let mut message = AgentMessage::user_text(text);
        message.role = role.to_string();
        message
    }

    /// 首轮结束事件（与 agent_loop 的载荷形状一致）。
    fn turn_end(scope: &str, reply: &str) -> Value {
        serde_json::json!({
            "type": "turn_end",
            "turnIndex": 0,
            "message": serde_json::to_value(message("assistant", reply)).unwrap(),
            "outcome": "completed",
            "agentScope": scope,
        })
    }

    #[test]
    fn normalize_title_strips_decorations_and_caps_length() {
        assert_eq!(normalize_title("  Title: \"Fix parser\"  "), "Fix parser");
        assert_eq!(normalize_title("`重写 会话 名`。"), "重写 会话 名");
        assert_eq!(normalize_title("a\n  b"), "a b", "换行与多空格归一");
        assert_eq!(normalize_title("trailing dot."), "trailing dot");
        let long = normalize_title(&"字".repeat(200));
        assert_eq!(
            long.chars().count(),
            MAX_CHARS,
            "硬截断到上限（省略号占配额）"
        );
    }

    #[test]
    fn fallback_title_uses_first_words() {
        assert_eq!(
            fallback_title("fix the parser\n\nand stuff"),
            "fix the parser and …"
        );
        assert_eq!(fallback_title("   "), EXT, "没有素材时退回扩展名");
        assert_eq!(
            fallback_title("@tmp/agent/repos/ 选定一个项目"),
            "选定一个项目",
            "@ 文件引用不得抢占名字预算"
        );
        assert_eq!(
            fallback_title("sweep /new path"),
            "sweep /new path",
            "斜杠命令不是路径"
        );
        let long = fallback_title(&"word ".repeat(100));
        assert!(long.chars().count() <= MAX_CHARS);
    }

    #[test]
    fn naming_prompt_includes_both_sides() {
        let prompt = naming_prompt("fix the parser", "Looking at it now.");
        assert!(prompt.contains("fix the parser"));
        assert!(prompt.contains("Looking at it now."));
        assert!(
            naming_prompt("", "only reply").contains("only reply"),
            "只有转录时也要能起名"
        );
    }

    #[test]
    fn new_session_keeps_state_when_its_file_appears() {
        let _g = lock_global();
        reset(false);
        lock().session = Some(None); // 刚 /new：会话还没有文件
        AutoTitle.on_user_submit("fix the parser", false);

        // /new 的新会话首次落盘：UI 补发一次切换，带的是当前转录而不是加载的历史。
        AutoTitle.on_session_switched(
            Some("/tmp/2026-11-13T07-08-48-995Z_abc.jsonl"),
            &[message("user", "fix the parser")],
        );
        assert!(!lock().skip, "/new 之后落盘不得被当成旧会话");
        assert_eq!(
            lock().prompt.as_deref(),
            Some("fix the parser"),
            "素材要留着，首轮结束才能命名"
        );
    }

    #[test]
    fn session_switch_with_messages_skips() {
        let _g = lock_global();
        AutoTitle.on_session_switched(None, &[message("user", "hi")]);
        assert!(lock().skip, "带消息恢复的会话不自动命名");
        AutoTitle.on_session_switched(None, &[]);
        assert!(!lock().skip, "空会话参与自动命名");
    }

    #[test]
    fn boundary_ignores_subagents_and_missing_prompt() {
        let _g = lock_global();
        reset(false);
        AutoTitle.on_user_submit("fix the parser", false);

        assert!(
            AutoTitle.on_boundary(&turn_end("subagent", "ok")).is_none(),
            "子代理不参与"
        );
        assert!(!lock().done, "子代理的事件不得把主 agent 标记成已命名");

        reset(false);
        assert!(
            AutoTitle.on_boundary(&turn_end("main", "ok")).is_none(),
            "没收到用户输入就不起名"
        );
    }

    #[test]
    fn boundary_marks_done_once_per_session() {
        let _g = lock_global();
        while take_pending_ui().is_some() {}
        reset(false);
        lock().model = Some(("anthropic".to_string(), "claude-fable-5".to_string()));
        AutoTitle.on_user_submit("fix the parser", false);

        // 出错的回合不拿来起名（素材是半截的），也不消耗那一次机会。
        let mut errored = turn_end("main", "");
        errored["outcome"] = serde_json::json!("error");
        assert!(AutoTitle.on_boundary(&errored).is_none());
        assert!(!lock().done, "出错回合不算起过名");

        // 无 tokio 运行时：请求发不出去，但"本会话只起一次"的判定仍要成立。
        AutoTitle.on_boundary(&turn_end("main", "Looking at it now"));
        assert!(lock().done, "首轮结束即标记已命名，不再重试");
        AutoTitle.on_boundary(&turn_end("main", "second turn"));
        while take_pending_ui().is_some() {}
    }

    #[test]
    fn set_persists_naming_model_and_bare_set_only_reminds_usage() {
        let _g = lock_global();
        let _dir = crate::test_support::AgentDirGuard::temp();
        let mut st = crate::modes::tui::app::App::new();

        command_title(
            &mut st,
            "auto-title set --provider anthropic --model claude-fable-5",
        );
        assert_eq!(
            config_model(),
            Some(("anthropic".to_string(), "claude-fable-5".to_string())),
            "配置应落盘并读得回来"
        );
        assert!(
            st.system_messages
                .iter()
                .any(|(_, m)| format!("{m:?}").contains("claude-fable-5")),
            "写入应有回执: {:?}",
            st.system_messages
        );

        // `--flag=value` 形式同样吃
        command_title(&mut st, "auto-title set --provider=openai --model=gpt-5");
        assert_eq!(
            config_model(),
            Some(("openai".to_string(), "gpt-5".to_string()))
        );

        // 裸 set 只提醒用法，不抹掉已配好的模型
        command_title(&mut st, "auto-title set");
        assert_eq!(
            config_model(),
            Some(("openai".to_string(), "gpt-5".to_string())),
            "裸 set 不得清配置"
        );
        assert!(
            st.system_messages
                .iter()
                .any(|(_, m)| format!("{m:?}").contains("usage")),
            "裸 set 应提醒用法: {:?}",
            st.system_messages
        );

        command_title(&mut st, "auto-title unset");
        assert_eq!(config_model(), None, "unset 才清配置");
    }

    #[test]
    fn non_subcommand_arguments_name_directly_without_a_request() {
        let _g = lock_global();
        let _dir = crate::test_support::AgentDirGuard::temp();
        while take_pending_ui().is_some() {}
        let mut st = crate::modes::tui::app::App::new();
        // 没选模型：手动起名不该理会模型，走到"没有模型"那条提示就说明它去调模型了。
        st.current_provider = None;
        st.current_model = None;

        command_title(&mut st, "auto-title my own   name");
        assert!(
            !st.system_messages
                .iter()
                .any(|(_, m)| format!("{m:?}").contains("No model")),
            "手动起名不得去解析模型: {:?}",
            st.system_messages
        );
        match take_pending_ui() {
            Some(ExtensionUiRequest::SetSessionName { name }) => {
                assert_eq!(name, "my own name", "参数压成一行后直接写进会话名");
            }
            other => panic!("应直接排入会话名请求，实际是 {other:?}"),
        }

        // 空参数才是"强制调模型起名"，此时没有模型才会提示。
        command_title(&mut st, "auto-title");
        assert!(
            st.system_messages
                .iter()
                .any(|(_, m)| format!("{m:?}").contains("No model")),
            "空参数应走去调模型的路径: {:?}",
            st.system_messages
        );
        assert_eq!(config_model(), None, "两条路径都不写配置");
    }

    #[test]
    fn set_subcommand_rejects_bad_input_without_touching_config() {
        let _g = lock_global();
        let _dir = crate::test_support::AgentDirGuard::temp();
        let mut st = crate::modes::tui::app::App::new();

        for raw in [
            "title set --provider anthropic",                   // 缺 model
            "title set --model claude-fable-5",                 // 缺 provider
            "title set --nope x",                               // 未知 flag
            "title set --provider anthropic --model",           // 缺值
            "title set --provider nope --model claude-fable-5", // provider 不存在
        ] {
            command_title(&mut st, raw);
        }
        assert_eq!(config_model(), None, "非法输入一律不写配置");
        assert!(
            st.system_messages.len() >= 5,
            "每条非法输入都要有提示: {:?}",
            st.system_messages
        );
    }

    #[test]
    fn configured_model_wins_over_the_session_model() {
        let _g = lock_global();
        let _dir = crate::test_support::AgentDirGuard::temp();

        lock().model = Some(("anthropic".to_string(), "claude-fable-5".to_string()));
        assert_eq!(
            naming_model(),
            Some(("anthropic".to_string(), "claude-fable-5".to_string())),
            "没配置时用当前会话模型"
        );

        store_model(Some(("openai", "gpt-5"))).unwrap();
        assert_eq!(
            naming_model(),
            Some(("openai".to_string(), "gpt-5".to_string())),
            "配了命名模型就用它"
        );
    }

    #[test]
    fn surface_is_declared() {
        assert_eq!(AutoTitle.name(), EXT);
        assert!(!AutoTitle.default_enabled(), "默认不启用");
        assert!(AutoTitle.tools().is_empty());
        assert_eq!(AutoTitle.commands()[0].name, CMD);
        assert_eq!(CMD, "title", "命令名是 /title，扩展名仍是 auto-title");
        assert_eq!(
            AutoTitle
                .commands()
                .remove(0)
                .subcommands
                .iter()
                .map(|s| s.name)
                .collect::<Vec<_>>(),
            vec![SUB_SET, SUB_UNSET]
        );
        assert_eq!(
            AutoTitle.hooks(),
            vec![ExtensionHook::Boundary, ExtensionHook::UserPrompt]
        );
    }
}
