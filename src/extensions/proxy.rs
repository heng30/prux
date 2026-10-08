//! `proxy` 扩展：把已登录的 provider 暴露成 **OpenAI 兼容**的本地网关。
//!
//! **方向提醒**：本扩展是入站网关（客户端 → prux → provider），
//! 与 [docs/http-proxy.md](../../../assets/docs/http-proxy.md) 描述的**出站** HTTP 代理
//! （`HTTP_PROXY` / `NO_PROXY`）方向相反、互不相干。
//!
//! 数据通路：
//! OpenAI 请求 → [`openai::parse_chat_request`] 转内部 `AgentMessage` →
//! [`crate::core::provider::stream_chat`]（鉴权 / OAuth 刷新 / 四协议适配）→
//! [`stream::ChunkWriter`] 回译成 OpenAI SSE chunk。
//!
//! 子命令：
//! - `/proxy listen <host:port>`：设置（并启动）监听地址；裸命令只显示当前状态；
//! - `/proxy provider <provider>`：设置目标 provider（校验已知 + 已配置凭据）；
//! - `/proxy api-key <sk-xx>`：设置入站 Bearer token；裸命令清空配置值（不鉴权）；
//! - `/proxy`：开关 dock 面板（监听地址 / 连接数 / 请求数 / 速率 / 最近请求）。
//!
//! 配置持久化在 `agent_dir()/extensions/proxy.json`，扩展启用即按配置自动监听
//! （需要 `listen` 与 `provider` 都已配置）。**默认不启用**，需在 `/extension` 面板开启。

mod config;
mod error;
mod openai;
mod server;
mod stats;
mod stream;

use crate::{
    core::{
        auth,
        extensions::{
            DockLine, DockSpan, Extension, ExtensionCommand, ExtensionHook, ExtensionMode,
            ExtensionTool, SubcommandDef, UiNotifyLevel,
        },
        model_resolver,
    },
    extensions::{
        EXTENSION_FACTORIES, ExtensionFactory, PRIORITY_PROXY, command_arg,
        util::{self, notify_text},
    },
    modes::interactive::{app::App, handlers::register_slash_command},
    utils::{
        display::{format_bytes, format_rate},
        glyphs::{DEF_DOT_EMPTY, DEF_DOT_FILLED, DEF_INPUT_TOKENS, DEF_OUTPUT_TOKENS},
        net,
        time::{format_clock, format_elapsed},
    },
};
use config::ProxyConfig;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use tokio::{
    net::TcpListener,
    sync::watch::{self, Sender},
};

pub use server::{MAX_BODY_BYTES, MAX_CONCURRENT, Runtime, serve};

/// 扩展名
const EXT: &str = "proxy";
/// 斜杠命令名
const CMD: &str = "proxy";
/// dock 里展示的最近请求条数
const DOCK_LOG_LINES: usize = 5;

/// 子命令候选元数据（顺序 = 输入框面板的候选顺序，也是空查询时的默认选中项）。
/// 与 `command_proxy` 的解析分支一一对应，`subcommands_match_parser` 测试钉死这一点。
const SUBCOMMANDS: &[SubcommandDef] = &[
    SubcommandDef {
        name: "listen",
        description: "Set the listen address: /proxy listen <host:port> (bare shows status)",
    },
    SubcommandDef {
        name: "provider",
        description: "Set the target provider: /proxy provider <provider> (bare shows status)",
    },
    SubcommandDef {
        name: "api-key",
        description: "Set the inbound Bearer token: /proxy api-key <sk-xx> (bare clears it)",
    },
];

/// 扩展工厂（分布式切片注册）：`main.rs` 启动时据此构造 [`Proxy`]
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static PROXY_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_PROXY,
    make: || -> Arc<dyn Extension> { Arc::new(Proxy) },
};

/// `/proxy` 子命令解析结果；与 [`SUBCOMMANDS`] 的候选一一对应。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    /// 裸 `/proxy`：开关 dock 面板
    ToggleDock,
    /// `/proxy listen` / `/proxy provider` 缺参数：只显示当前状态
    ShowStatus,
    /// `/proxy listen <host:port>`：设置监听地址
    Listen,
    /// `/proxy provider <provider>`：设置上游 provider
    Provider,
    /// `/proxy api-key <sk-xx>`：设置入站 Bearer token（裸命令则清空）
    ApiKey,
}

/// 取进程级代理扩展状态单例（首次访问时惰性初始化，配置从磁盘加载）。
fn state() -> &'static Mutex<State> {
    /// 进程级代理扩展状态单例，首次访问时惰性初始化。
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(State::new()))
}

/// 取互斥锁；即使持锁线程 panic 导致锁中毒也取回内部数据，避免扩展整体卡死。
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// 正在运行的监听实例
struct Running {
    /// 实际监听地址（`host:port`），dock 与状态提示展示用
    addr: String,
    /// 置位（或全部 drop）即关停接受循环与在途连接
    shutdown: Sender<bool>,
}

impl Running {
    /// 置位 shutdown 信号，令接受循环与在途连接退出；消费 self，停服后实例不可复用。
    fn stop(self) {
        _ = self.shutdown.send(true);
    }
}

/// 扩展状态（进程内单例）
struct State {
    /// 当前生效的持久化配置（`proxy.json` 的内存副本）
    config: ProxyConfig,
    /// 服务端运行时（provider / token / 并发计数），与扩展生命周期解耦
    runtime: Arc<Runtime>,
    /// 正在运行的监听实例；`None` = 未监听
    server: Option<Running>,
    /// dock 段的显隐（内存态，不持久化）
    dock_shown: bool,
}

impl State {
    /// 按磁盘配置构造初始状态：加载 `proxy.json` 并据此初始化运行时 provider/token；
    /// 初始未监听、dock 段隐藏。
    fn new() -> Self {
        let config = config::load();
        let runtime = Arc::new(Runtime::new(
            config.provider.clone(),
            config::token(&config),
        ));
        Self {
            config,
            runtime,
            server: None,
            dock_shown: false,
        }
    }
}

/// `proxy` 扩展入口（`main.rs` 经分布式切片注册）
pub struct Proxy;

impl Extension for Proxy {
    /// 扩展名固定为 `"proxy"`（配置键与 `/extension` 面板都用它）。
    fn name(&self) -> &str {
        EXT
    }

    /// 一句话描述：OpenAI 兼容本地网关及三个子命令。
    fn description(&self) -> &str {
        "OpenAI-compatible local gateway: /proxy listen <host:port>, /proxy provider <provider>, /proxy api-key <sk-xx> (streams to the configured provider; /proxy toggles the stats panel)."
    }

    /// 仅在 Dev / Creator 两种模式下提供。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Dev, ExtensionMode::Creator]
    }

    /// 默认关闭：会占用端口并把已登录凭据暴露成 HTTP 端点，必须显式开启。
    fn default_enabled(&self) -> bool {
        // 会占用端口、并把已登录凭据暴露成 HTTP 端点 → 必须显式开启
        false
    }

    /// 不提供任何模型可见工具（纯基础设施扩展）。
    fn tools(&self) -> Vec<ExtensionTool> {
        // 纯基础设施扩展：不给模型任何工具
        Vec::new()
    }

    /// 声明唯一的斜杠命令 `/proxy`：标记 busy_safe，并附上三个子命令候选。
    fn commands(&self) -> Vec<ExtensionCommand> {
        vec![ExtensionCommand {
            name: CMD.to_string(),
            description: "/proxy toggles the gateway stats panel; /proxy listen <host:port>; /proxy provider <provider>; /proxy api-key <sk-xx>"
                .to_string(),
            busy_safe: true, // handler 不锁 agent（只改扩展状态 + 发 UI 请求），忙碌时可安全执行
            subcommands: SUBCOMMANDS.to_vec(),
        }]
    }

    /// 注册 `/proxy` 的 TUI 执行入口（幂等，重复注册覆盖旧接线）。
    fn on_registered(&self) {
        register_slash_command(EXT, CMD, command_proxy);
    }

    /// 启用即按配置自动监听；禁用即停服（配置保留）
    fn on_enabled_changed(&self, enabled: bool) {
        let auto_listen = {
            let mut st = lock(state());
            let cfg = config::load();
            st.config = cfg.clone();
            st.runtime.set_provider(cfg.provider.clone());
            st.runtime.set_token(config::token(&cfg));

            if !enabled {
                if let Some(server) = st.server.take() {
                    server.stop();
                }
                stats::mark_stopped();
                st.dock_shown = false;
                return;
            }

            auto_listen_addr(&cfg)
        };

        if let Some(addr) = auto_listen {
            spawn_listen(addr);
        }
    }

    /// 只有面板显示时才订阅 Dock（否则框架每帧空转）
    fn hooks(&self) -> Vec<ExtensionHook> {
        if lock(state()).dock_shown {
            vec![ExtensionHook::Dock]
        } else {
            Vec::new()
        }
    }

    /// 转发到模块级 `dock_lines`：面板未显示时返回空，框架便不渲染这一段。
    fn dock_lines(&self) -> Vec<DockLine> {
        dock_lines()
    }

    /// 面板显示时请求每帧重绘：网关在后台持续更新计数 / 速率 / 最近请求，
    /// 而空闲（非 busy）时 TUI 不重绘，面板会冻结到下一次按键。
    fn wants_redraw(&self) -> bool {
        lock(state()).dock_shown
    }
}

/// 自动启动条件：`listen` 与 `provider` 都已配置。
///
/// 只配了地址就起监听，只会让每个请求都拿到 `400 no provider configured`——
/// 与其开一个注定报错的端口，不如让它保持未监听（dock 里会提示该配什么）。
fn auto_listen_addr(cfg: &ProxyConfig) -> Option<String> {
    cfg.listen.clone().filter(|_| cfg.provider.is_some())
}

/// `/proxy` 命令入口：解析子命令后分派给对应处理器，未知子命令只提示用法。
/// 始终返回 false（不请求退出程序）。
fn command_proxy(app: &mut App, raw: &str) -> bool {
    let arg = command_arg(raw).trim();
    let (sub, rest) = match arg.split_once(char::is_whitespace) {
        Some((a, b)) => (a, b.trim()),
        None => (arg, ""),
    };

    let Some(action) = parse_action(sub, rest) else {
        notify_text(
            &format!(
                "Proxy: unknown subcommand `{sub}`. Usage: /proxy listen <host:port> | /proxy provider <provider> | /proxy api-key <sk-xx> | /proxy"
            ),
            UiNotifyLevel::Warning,
        );
        return false;
    };

    match action {
        Action::ToggleDock => toggle_dock(app),
        Action::ShowStatus => show_status(),
        Action::Listen => set_listen(rest),
        Action::Provider => set_provider(rest),
        Action::ApiKey => set_api_key(rest),
    }
    false
}

/// 把子命令名（+ 是否有参数）映射到 [`Action`]；未知子命令返回 `None`。
/// 空参数对 `listen` / `provider` 是「看状态」，对 `api-key` 是「清空」。
fn parse_action(sub: &str, rest: &str) -> Option<Action> {
    let bare = rest.is_empty();
    match sub {
        "" => Some(Action::ToggleDock),
        "listen" if bare => Some(Action::ShowStatus),
        "listen" => Some(Action::Listen),
        "provider" if bare => Some(Action::ShowStatus),
        "provider" => Some(Action::Provider),
        "api-key" => Some(Action::ApiKey),
        _ => None,
    }
}

/// 裸 `/proxy`：三分支开关 dock 段（对齐 plan-mode `/todos`）
fn toggle_dock(app: &mut App) {
    if !app.dock_visible {
        lock(state()).dock_shown = true;
        app.dock_visible = true;
        app.dirty = true;
        return;
    }

    {
        let mut st = lock(state());
        st.dock_shown = !st.dock_shown;
    }
    app.dirty = true;
}

/// 裸子命令（`/proxy listen` / `/proxy provider` 无参数）时提示当前监听地址、
/// 配置地址、目标 provider 与鉴权状态。
fn show_status() {
    let st = lock(state());
    let running = st
        .server
        .as_ref()
        .map(|s| s.addr.clone())
        .unwrap_or_else(|| "(not listening)".to_string());
    let provider = st
        .config
        .provider
        .clone()
        .unwrap_or_else(|| "(not set)".to_string());
    let listen_cfg = st
        .config
        .listen
        .clone()
        .unwrap_or_else(|| "(not set)".to_string());
    let auth = config::token(&st.config)
        .map(|_| "bearer token required")
        .unwrap_or("no auth");
    util::notify_text(
        &format!(
            "Proxy: listening on {running} · configured {listen_cfg} · provider {provider} · {auth}. Usage: /proxy listen <host:port> | /proxy provider <provider> | /proxy api-key <sk-xx> | /proxy"
        ),
        UiNotifyLevel::Info,
    );
}

/// `/proxy listen <host:port>`：校验并归一化地址，写入配置后重新起监听；
/// 地址非法或与当前监听相同则只提示、不改状态。
fn set_listen(raw: &str) {
    let addr = match net::normalize_listen(raw) {
        Ok(a) => a,
        Err(e) => {
            notify_text(&format!("Proxy: {e}"), UiNotifyLevel::Error);
            return;
        }
    };

    let cfg = {
        let mut st = lock(state());
        if let Some(server) = st.server.as_ref()
            && server.addr == addr
        {
            notify_text(
                &format!("Proxy: already listening on {addr}"),
                UiNotifyLevel::Info,
            );
            return;
        }
        st.config.listen = Some(addr.clone());
        st.config.clone()
    };
    config::save(&cfg);

    // 先起新的、绑定成功才停旧的（绑定失败则旧监听继续服务）
    spawn_listen(addr);
}

/// 起监听：绑定在 tokio 任务里完成（`TcpListener::bind` 是异步的）。
///
/// 顺序刻意是「bind 成功 → 停旧 → 装新」而非「先停旧再 bind」：同一端口重新监听时
/// 旧监听仍占着端口，先停后 bind 会让失败窗口暴露给客户端；而 bind 失败时保留旧监听，
/// 语义上只是「这次没换成功」。
fn spawn_listen(addr: String) {
    let runtime = lock(state()).runtime.clone();
    let (tx, rx) = watch::channel(false);

    tokio::spawn(async move {
        let listener = match TcpListener::bind(&addr).await {
            Ok(l) => l,
            Err(e) => {
                notify_text(
                    &format!("Proxy: cannot listen on {addr}: {e}"),
                    UiNotifyLevel::Error,
                );
                return;
            }
        };

        {
            let mut st = lock(state());
            if let Some(old) = st.server.take() {
                old.stop();
            }
            st.server = Some(Running {
                addr: addr.clone(),
                shutdown: tx,
            });
        }
        stats::reset();

        let provider_missing = lock(state()).config.provider.is_none();
        notify_text(
            &format!(
                "Proxy: listening on {addr} · Base URL: {}{}",
                net::base_url(&addr),
                if provider_missing {
                    " (no provider configured yet: /proxy provider <provider>)"
                } else {
                    ""
                }
            ),
            if provider_missing {
                UiNotifyLevel::Warning
            } else {
                UiNotifyLevel::Success
            },
        );

        server::serve(listener, runtime, rx).await;
    });
}

/// `/proxy provider <provider>`：要求 provider 已知且已有存储凭据，
/// 通过后写入配置、热更新运行时的 provider，并提示可用模型数。
fn set_provider(raw: &str) {
    let provider = raw.trim();
    if !model_resolver::is_known_provider(provider) {
        notify_text(
            &format!("Proxy: unknown provider `{provider}`"),
            UiNotifyLevel::Error,
        );
        return;
    }

    if !auth::list_configured_providers()
        .iter()
        .any(|p| p == provider)
    {
        notify_text(
            &format!("Proxy: provider `{provider}` has no stored credentials; run /login first"),
            UiNotifyLevel::Error,
        );
        return;
    }

    let models = model_resolver::list_models(provider).len();
    let (runtime, cfg) = {
        let mut st = lock(state());
        st.config.provider = Some(provider.to_string());
        (st.runtime.clone(), st.config.clone())
    };

    runtime.set_provider(Some(provider.to_string()));
    config::save(&cfg);

    notify_text(
        &format!("Proxy: provider = {provider} ({models} models available to request)"),
        UiNotifyLevel::Success,
    );
}

/// `/proxy api-key <sk-xx>`：写入/清空配置文件 `apiKey`，并热更新运行时 token。
///
/// 裸命令（`rest` 为空）= 清空配置值（关掉鉴权）；但 `PRUX_PROXY_TOKEN` 仍然优先，
/// 所以提示里要把它标出来，避免用户以为「清空了却还在要鉴权」。
fn set_api_key(raw: &str) {
    let key = raw.trim();
    let key = (!key.is_empty()).then(|| key.to_string());

    let (runtime, cfg) = {
        let mut st = lock(state());
        st.config.api_key = key.clone();
        (st.runtime.clone(), st.config.clone())
    };

    // token 解析含环境变量优先级：运行时与配置文件保持一致地走 `config::token`
    runtime.set_token(config::token(&cfg));
    config::save(&cfg);

    let (msg, level) = match config::token_with_source(&cfg) {
        None => (
            "Proxy: api-key cleared; gateway now accepts requests without auth".to_string(),
            UiNotifyLevel::Warning,
        ),
        Some((_, config::TokenSource::Env)) => (
            format!(
                "Proxy: api-key {} · {} is set and takes precedence",
                if key.is_some() {
                    "saved to config"
                } else {
                    "cleared from config"
                },
                config::TOKEN_ENV
            ),
            UiNotifyLevel::Warning,
        ),
        Some((_, config::TokenSource::Config)) => (
            "Proxy: api-key set; gateway now requires Bearer auth".to_string(),
            UiNotifyLevel::Success,
        ),
    };
    notify_text(&msg, level);
}

/// 组装 dock 面板内容：标题行（地址 / provider / 遮蔽后的 api-key / 运行时长）、
/// 连接与请求计数、token 与流量、以及最近几次请求日志。
/// 面板未显示时返回空 Vec；未监听时只给标题 + 启动提示。
fn dock_lines() -> Vec<DockLine> {
    let (addr, configured, provider, api_key) = {
        let st = lock(state());
        if !st.dock_shown {
            return Vec::new();
        }

        (
            st.server.as_ref().map(|s| s.addr.clone()),
            st.config.listen.clone(),
            st.config
                .provider
                .clone()
                .unwrap_or_else(|| "(no provider)".to_string()),
            api_key_label(&st.config),
        )
    };

    let snap = stats::snapshot();

    // 第 1 行：标题 + 状态 + provider + api-key（遮蔽）+ 运行时长
    let mut title = vec![DockSpan::plain("Proxy")];
    match &addr {
        Some(a) => title.push(DockSpan::new(
            "success",
            "#b5bd68",
            format!("  {DEF_DOT_FILLED} {a}"),
        )),
        None => title.push(DockSpan::new(
            "dim",
            "#666666",
            format!("  {DEF_DOT_EMPTY} not listening"),
        )),
    }
    title.push(DockSpan::new("muted", "#999999", format!("  · {provider}")));
    title.push(DockSpan::new("muted", "#999999", format!("  · {api_key}")));
    if snap.running {
        title.push(DockSpan::new(
            "dim",
            "#666666",
            format!("  · up {}", format_elapsed(snap.uptime_s)),
        ));
    }

    if addr.is_none() {
        let hint = match configured {
            Some(a) => format!("  /proxy listen {a} to start (configured address)"),
            None => "  /proxy listen <host:port> · /proxy provider <provider>".to_string(),
        };
        return vec![title, vec![DockSpan::new("dim", "#666666", hint)]];
    }

    let mut out = vec![title];
    out.push(vec![DockSpan::new(
        "muted",
        "#999999",
        format!(
            "  conns {} active / {} total · requests {} running / {} ok / {} failed",
            snap.conns_active,
            snap.conns_total,
            snap.requests_active,
            snap.requests_ok,
            snap.requests_failed
        ),
    )]);
    out.push(vec![DockSpan::new(
        "muted",
        "#999999",
        format!(
            "  tokens {} in / {} out · {DEF_INPUT_TOKENS} {} ({}) · {DEF_OUTPUT_TOKENS} {} ({})",
            util::format_tokens(snap.tokens_in),
            util::format_tokens(snap.tokens_out),
            format_bytes(snap.bytes_in),
            format_rate(snap.bytes_in_rate),
            format_bytes(snap.bytes_out),
            format_rate(snap.bytes_out_rate)
        ),
    )]);

    for entry in stats::recent(DOCK_LOG_LINES) {
        let key = if entry.status >= 400 { "error" } else { "dim" };
        let fallback = if entry.status >= 400 {
            "#cc6666"
        } else {
            "#666666"
        };
        out.push(vec![DockSpan::new(
            key,
            fallback,
            format!(
                "  {}  {}  {:.1}s  {}  {} tok",
                format_clock(entry.at_ms),
                util::truncate_objective(&entry.model, 32),
                entry.ms as f64 / 1000.0,
                entry.status,
                entry.tokens_out
            ),
        )]);
    }
    out
}

/// dock 第 1 行的入站 token 展示串：`api-key <遮蔽值>`。
///
/// 展示的是**生效中**的那个 token（`PRUX_PROXY_TOKEN` 优先于配置文件 `apiKey`），
/// 并用 `(env)` 标出来源：否则环境变量还在覆盖时，用户改了 `apiKey` 会看起来「没生效」。
/// 具体值走 [`util::mask_secret`]（首尾各留几个字符，中间 5 个 `*`），面板上不放明文。
fn api_key_label(cfg: &ProxyConfig) -> String {
    match config::token_with_source(cfg) {
        None => "api-key (none)".to_string(),
        Some((token, config::TokenSource::Env)) => {
            format!("api-key {} (env)", util::mask_secret(&token))
        }
        Some((token, config::TokenSource::Config)) => {
            format!("api-key {}", util::mask_secret(&token))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{AgentDirGuard, env_key_lock};

    /// 状态是进程内单例、测试并行：所有触碰它的用例共用一把锁串行
    fn serial() -> MutexGuard<'static, ()> {
        static L: OnceLock<Mutex<()>> = OnceLock::new();
        lock(L.get_or_init(|| Mutex::new(())))
    }

    fn reset_state() {
        let mut st = lock(state());
        st.server = None;
        st.dock_shown = false;
        st.config = config::ProxyConfig::default();
        st.runtime.set_provider(None);
        stats::mark_stopped();
    }

    fn fake_running(addr: &str) {
        let (tx, _rx) = watch::channel(false);
        let mut st = lock(state());
        st.server = Some(Running {
            addr: addr.to_string(),
            shutdown: tx,
        });
    }

    #[test]
    fn extension_metadata_matches_contract() {
        let ext = Proxy;
        assert_eq!(ext.name(), "proxy");
        assert!(!ext.default_enabled(), "默认关闭：占用端口 + 暴露凭据");
        assert_eq!(
            ext.modes(),
            vec![ExtensionMode::Dev, ExtensionMode::Creator],
            "与 web-access 同级别的按需扩展"
        );
        assert!(ext.tools().is_empty(), "不提供模型可见工具");
        assert_eq!(ext.commands().len(), 1);
        assert_eq!(ext.commands()[0].name, "proxy");
        assert!(ext.commands()[0].busy_safe, "handler 不锁 agent");
        assert_eq!(
            ext.commands()[0].subcommands,
            SUBCOMMANDS.to_vec(),
            "输入框面板应提示这三个子命令"
        );
    }

    /// 输入框面板提示的子命令必须都能被解析器执行（否则用户按 Tab/Enter
    /// 补全出一个不生效的子命令）。SUBCOMMANDS 与 [`parse_action`] 一一对应。
    #[test]
    fn subcommands_match_parser() {
        let declared: Vec<&str> = SUBCOMMANDS.iter().map(|s| s.name).collect();
        assert_eq!(
            declared,
            vec!["listen", "provider", "api-key"],
            "候选顺序固定"
        );

        // 每个候选带参数都应解析到对应动作（而不是 unknown）
        assert_eq!(parse_action("listen", "127.0.0.1:1"), Some(Action::Listen));
        assert_eq!(parse_action("provider", "deepseek"), Some(Action::Provider));
        assert_eq!(parse_action("api-key", "sk-x"), Some(Action::ApiKey));

        // listen / provider 裸命令 = 看状态；api-key 裸命令 = 清空（仍算已知子命令）
        assert_eq!(parse_action("listen", ""), Some(Action::ShowStatus));
        assert_eq!(parse_action("provider", ""), Some(Action::ShowStatus));
        assert_eq!(parse_action("api-key", ""), Some(Action::ApiKey));

        // 裸命令与未知子命令
        assert_eq!(parse_action("", ""), Some(Action::ToggleDock));
        assert_eq!(parse_action("bogus", "x"), None);
    }

    #[test]
    fn dock_section_follows_shown_state_and_reports_status() {
        // 先拿统计串行锁（库代码从不取测试锁，故无锁序死锁风险）
        let _stats = stats::serial();
        let _l = serial();
        let _g = AgentDirGuard::temp();
        reset_state();

        assert!(dock_lines().is_empty(), "未显示时无内容");

        {
            let mut st = lock(state());
            st.dock_shown = true;
            st.config.provider = Some("deepseek".into());
        }
        let lines = dock_lines();
        assert_eq!(lines.len(), 2, "未监听：标题 + 启动提示");
        assert!(lines[0][0].text.contains("Proxy"));
        assert!(lines[0][1].text.contains("not listening"));
        assert!(lines[0][2].text.contains("deepseek"));

        fake_running("127.0.0.1:8765");
        stats::reset();
        stats::conn_opened();
        stats::request_started();
        stats::request_finished(true);
        stats::add_bytes_in(2048);
        stats::add_bytes_out(4096);
        stats::add_tokens(100, 200);

        let lines = dock_lines();
        assert!(
            lines[0][1].text.contains("127.0.0.1:8765"),
            "{:?}",
            lines[0]
        );
        assert!(lines[0][1].key == "success");
        let joined: Vec<String> = lines.iter().map(|l| l[0].text.clone()).collect();
        assert!(
            joined[1].contains("conns 1 active / 1 total"),
            "{:?}",
            joined
        );
        assert!(joined[1].contains("1 ok / 0 failed"), "{:?}", joined);
        // token 与流量合并为一行，两段以 `·` 分隔
        assert!(joined[2].contains("100 in / 200 out"), "{:?}", joined);
        assert!(joined[2].contains("KiB"), "{:?}", joined);
        assert!(
            joined[2].contains("200 out · ↑"),
            "token 与流量之间应有 `·`：{:?}",
            joined
        );

        // 请求日志行
        stats::push_log(stats::LogEntry {
            at_ms: 3_661_000,
            model: "deepseek-chat".into(),
            ms: 1200,
            status: 200,
            tokens_out: 342,
        });
        stats::push_log(stats::LogEntry {
            at_ms: 3_662_000,
            model: "deepseek-chat".into(),
            ms: 30,
            status: 502,
            tokens_out: 0,
        });
        let lines = dock_lines();
        let last_two: Vec<String> = lines[lines.len() - 2..]
            .iter()
            .map(|l| l[0].text.clone())
            .collect();
        let expect_first = crate::utils::time::format_clock(3_662_000);
        assert!(
            last_two[0].contains(&expect_first),
            "{:?} 应含本地时间 {expect_first}",
            last_two
        );
        assert!(last_two[0].contains("502"), "{:?}", last_two);
        assert_eq!(lines[lines.len() - 2][0].key, "error", "失败请求用错误色");
        assert!(last_two[1].contains("200"), "{:?}", last_two);

        reset_state();
        stats::reset();
    }

    /// 临时移去/改写 `PRUX_PROXY_TOKEN` 并在 Drop 时还原（配合 [`env_key_lock`] 串行）。
    /// 用 guard 而不是「断言完再还原」：中途 panic 就不会把环境变量漏给其他测试。
    struct TokenEnvGuard {
        _lock: MutexGuard<'static, ()>,
        saved: Option<String>,
    }

    impl TokenEnvGuard {
        /// 进入「环境变量未设置」状态（返回后先持锁、再清变量）
        fn cleared() -> Self {
            let lock = env_key_lock();
            let saved = std::env::var(config::TOKEN_ENV).ok();
            unsafe { std::env::remove_var(config::TOKEN_ENV) };
            Self { _lock: lock, saved }
        }

        fn set(&self, value: &str) {
            unsafe { std::env::set_var(config::TOKEN_ENV, value) };
        }

        fn clear(&self) {
            unsafe { std::env::remove_var(config::TOKEN_ENV) };
        }
    }

    impl Drop for TokenEnvGuard {
        fn drop(&mut self) {
            match self.saved.take() {
                Some(v) => unsafe { std::env::set_var(config::TOKEN_ENV, v) },
                None => unsafe { std::env::remove_var(config::TOKEN_ENV) },
            }
        }
    }

    /// dock 第 1 行里 `api-key` 那一段的文本
    fn api_key_span(lines: &[DockLine]) -> String {
        lines[0]
            .iter()
            .find(|s| s.text.contains("api-key"))
            .unwrap_or_else(|| panic!("第 1 行应有 api-key：{:?}", lines[0]))
            .text
            .trim()
            .to_string()
    }

    #[test]
    fn dock_masks_api_key_and_prefers_env_source() {
        let _env = TokenEnvGuard::cleared();
        let _stats = stats::serial();
        let _l = serial();
        let _g = AgentDirGuard::temp();
        reset_state();

        lock(state()).dock_shown = true;
        assert_eq!(api_key_span(&dock_lines()), "· api-key (none)", "未配置");

        // 配置文件来源：首尾各 4 个字符 + 中间 5 个 `*`，且不出现明文
        let key = "sk-proj-abcdefghijklmnop";
        lock(state()).config.api_key = Some(key.to_string());
        let span = api_key_span(&dock_lines());
        assert_eq!(span, "· api-key sk-p*****mnop");
        assert!(!span.contains("abcdefghijkl"), "面板不得放明文：{span}");
        assert!(!span.contains("(env)"), "配置文件来源不标 env：{span}");

        // 短 key（< 12）首尾全遮：否则遮蔽形同虚设
        lock(state()).config.api_key = Some("sk-short".into());
        assert_eq!(api_key_span(&dock_lines()), "· api-key *****");

        // 环境变量优先，并标出来源（否则改了 api_key 会看起来「没生效」）
        lock(state()).config.api_key = Some(key.to_string());
        _env.set("env-token-0123456789");
        let span = api_key_span(&dock_lines());
        assert_eq!(span, "· api-key env-*****6789 (env)");
        assert!(!span.contains("token-0123"), "面板不得放明文：{span}");

        // 环境变量清空 → 回到配置文件
        _env.clear();
        assert_eq!(api_key_span(&dock_lines()), "· api-key sk-p*****mnop");

        reset_state();
    }

    #[test]
    fn api_key_subcommand_sets_and_clears_config() {
        let _env = TokenEnvGuard::cleared();
        let _stats = stats::serial();
        let _l = serial();
        let _g = AgentDirGuard::temp();
        reset_state();

        // 设置：写入内存 + 落盘，并热更新运行时 token
        set_api_key(" sk-test-1234 ");
        assert_eq!(
            lock(state()).config.api_key.as_deref(),
            Some("sk-test-1234")
        );
        assert_eq!(config::load().api_key.as_deref(), Some("sk-test-1234"));
        assert_eq!(
            lock(state()).runtime.token().as_deref(),
            Some("sk-test-1234")
        );

        // 裸命令（空白）：清空配置值，运行时不再要求鉴权
        set_api_key("   ");
        assert!(lock(state()).config.api_key.is_none());
        assert!(config::load().api_key.is_none());
        assert!(lock(state()).runtime.token().is_none());

        reset_state();
    }

    #[test]
    fn hooks_subscribe_dock_only_when_shown() {
        let _l = serial();
        let _g = AgentDirGuard::temp();
        reset_state();
        assert!(Proxy.hooks().is_empty(), "隐藏时不订阅 Dock");
        assert!(!Proxy.wants_redraw(), "隐藏时无需额外重绘");

        lock(state()).dock_shown = true;
        assert_eq!(Proxy.hooks(), vec![ExtensionHook::Dock]);
        assert!(Proxy.wants_redraw(), "面板显示时即使空闲也要请求重绘");

        reset_state();
    }

    #[test]
    fn disabled_extension_stops_server_and_hides_dock() {
        let _l = serial();
        let _g = AgentDirGuard::temp();
        reset_state();
        {
            let mut st = lock(state());
            st.dock_shown = true;
            st.config.provider = Some("deepseek".into());
            st.config.listen = Some("127.0.0.1:8765".into());
        }
        fake_running("127.0.0.1:8765");
        stats::reset();
        assert!(stats::snapshot().running);

        Proxy.on_enabled_changed(false);

        let st = lock(state());
        assert!(st.server.is_none(), "禁用应停服");
        assert!(!st.dock_shown, "禁用应隐藏 dock 段");
        drop(st);
        assert!(!stats::snapshot().running);

        reset_state();
    }

    #[test]
    fn auto_listen_requires_both_listen_and_provider() {
        let cfg = |listen: Option<&str>, provider: Option<&str>| config::ProxyConfig {
            listen: listen.map(str::to_string),
            provider: provider.map(str::to_string),
            api_key: None,
        };
        assert_eq!(
            auto_listen_addr(&cfg(Some("127.0.0.1:8765"), Some("deepseek"))).as_deref(),
            Some("127.0.0.1:8765")
        );
        assert_eq!(
            auto_listen_addr(&cfg(Some("127.0.0.1:8765"), None)),
            None,
            "只配地址不起监听"
        );
        assert_eq!(auto_listen_addr(&cfg(None, Some("deepseek"))), None);
        assert_eq!(auto_listen_addr(&config::ProxyConfig::default()), None);
    }

    #[test]
    fn command_toggles_dock_and_reports_status_without_panicking() {
        let _stats = stats::serial();
        let _l = serial();
        let _g = AgentDirGuard::temp();
        reset_state();

        // 对齐 plan-mode `/todos` 的三分支：未开面板 → 显示并打开；显示中 → 隐藏；隐藏 → 重新显示
        let mut app = App::new();
        assert!(!app.dock_visible);
        command_proxy(&mut app, "/proxy");
        assert!(app.dock_visible, "应打开面板");
        assert!(lock(state()).dock_shown, "应显示本段");
        assert!(app.dirty, "应请求重绘");

        command_proxy(&mut app, "/proxy");
        assert!(!lock(state()).dock_shown, "第二次应隐藏本段");

        command_proxy(&mut app, "/proxy");
        assert!(lock(state()).dock_shown, "第三次应重新显示");

        // 未知子命令只提示、不改状态（这里刻意不触发 listen/provider 的 UI 通知路径：
        // 它们会往全局 UI 队列投消息，而队列是同进程各扩展测试共享的）
        assert!(lock(state()).server.is_none(), "只读命令不该起监听");
        assert!(lock(state()).config.listen.is_none());
        assert!(lock(state()).config.provider.is_none());

        reset_state();
    }
}
