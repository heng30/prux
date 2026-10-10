//! `doctor` 扩展：provider 网络可达性诊断（`/doctor`、`/doctor summary`）。
//!
//! 只做**基础设施体检**：对每个 provider 取它的第一个 chat 模型，用该模型的
//! `base_url` 发一个不带任何凭据的 `HEAD` 请求。目的只是判断端点是否活着，
//! 不产生计费请求、不写服务端日志，也不校验鉴权是否配置正确。
//!
//! 判定口径（见 [`reachable_detail`] / [`classify_error`]）：
//! - **任何 HTTP 响应都算网络可达**——探的是 provider 的 base URL 而不是真实 API 路径，
//!   `404` / `405` 这类"这个路径没有处理器"的响应同样证明连接与 TLS 都通，而"能不能出得去"
//!   正是防火墙排障要问的问题；`2xx` / `401` / `403` / `405` 在行里不加附注，
//!   其它状态码标 `status not OK`，让"网通但服务端不对"一眼可见；
//! - 只有**请求本身失败**（超时 / DNS / 连接 / TLS）才记不可达；
//! - 超时 → `timeout`；DNS/连接/TLS 失败 → 简短原因（`dns error` / `connect error`）。
//!
//! 面板内容：`/doctor` 每行一个 provider，`/doctor summary` 只给 `reachable / failed`
//! 计数与失败清单；两档首行都是 [`title_line`] 的顶栏（面板样式不渲染 `title` 字段，
//! 标题只能落在内容首行，同 cache-usage），末段都附 [`environment_lines`] 的本地环境段，
//! 代理变量有无取自 [`crate::utils::proxy::env_flags`]（与请求实际读取的变量、大小写规则一致）。
//! 离线模式（[`crate::core::runtime::offline`]）下不发任何请求，只提示
//! `offline mode: network probes skipped`。

use crate::{
    APP_NAME,
    core::{
        extensions::{
            DockLine, DockSpan, Extension, ExtensionCommand, ExtensionHook, ExtensionMode,
            ExtensionTool, ExtensionUiRequest, OverlayEvent, OverlaySize, OverlayView,
            SubcommandDef, next_ui_id, request_ui,
        },
        model_resolver, runtime, settings_manager,
    },
    extensions::{
        ACCENT, DIM, ERROR, EXTENSION_FACTORIES, ExtensionFactory, PRIORITY_DOCTOR, SUCCESS,
        WARNING, command_arg, util::default_user_agent,
    },
    modes::interactive::{
        app::{App, MsgLevel},
        handlers::register_slash_command,
    },
    utils::{glyphs::DEF_ARROW_UP_DOWN, http, proxy},
};
use futures_util::StreamExt;
use std::{
    sync::{Arc, Mutex, MutexGuard, OnceLock},
    time::Duration,
};

/// 扩展名。
const EXT: &str = "doctor";

/// 单个探测请求的超时。
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// 并发探测的 provider 数上限（同时打开的在途请求数）。
const PROBE_CONCURRENCY: usize = 16;

/// 覆盖面板可见行上限。
const MAX_PANEL_ROWS: usize = 20;

/// 自声明工厂：linkme 分布式切片。
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static DOCTOR_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_DOCTOR,
    make: || -> Arc<dyn Extension> { Arc::new(Doctor) },
};

/// 一个 provider 的探测目标。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Target {
    /// provider 标识（`catalog_providers()` 的一项）。
    provider: String,
    /// 该 provider 第一个 chat 模型的请求协议（如 `anthropic-messages`）。
    api: String,
    /// 探测地址：该模型的 `base_url`（HEAD 请求直接打这里）。
    base_url: String,
    /// 从 [`Target::base_url`] 解析出的 `host[:port]`（不含 scheme / userinfo / path）。
    host: String,
}

/// 单个 provider 的探测结论。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome {
    /// 端点可达，携带 HTTP 状态码（2xx / 401 / 403）。
    Reachable(u16),
    /// 请求失败（`timeout` / `dns error` / `connect error` 等）：网络层就出不去。
    Unreachable(String),
}

/// 一个目标的探测结果：目标身份 + 结论。
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProbeResult {
    /// provider 标识。
    provider: String,
    /// 请求协议。
    api: String,
    /// `host[:port]`。
    host: String,
    /// 探测结论。
    outcome: Outcome,
}

impl ProbeResult {
    /// 由目标与结论构造结果（复制目标的展示字段）。
    fn new(target: &Target, outcome: Outcome) -> Self {
        Self {
            provider: target.provider.clone(),
            api: target.api.clone(),
            host: target.host.clone(),
            outcome,
        }
    }
}

/// 扩展 UI 状态（进程级单例）。
#[derive(Default)]
struct State {
    /// 当前覆盖层 id（None = 未显示）。
    overlay_id: Option<u64>,
    /// 探测结果（None = 尚在探测，面板显示 `probing…`）。
    results: Option<Vec<ProbeResult>>,
    /// 跳过网络探测的原因（离线 / 无 tokio 运行时）；None = 正常探测。
    skip_reason: Option<String>,
    /// 当前面板是否为 summary 视图。
    summary: bool,
    /// 探测进行中（驱动面板文案与重绘）。
    probing: bool,
    /// 结果刚就绪、还没渲染过：置位后至少再要一帧重绘，面板认领内容时清除。
    ///
    /// 空闲时 TUI 只在 80ms tick 里轮询 [`Extension::wants_redraw`]，探测若碰巧
    /// 落在两次 tick 之间完成，`probing` 已经归 false，没有这个标志就再也等不到重绘
    /// （界面停在 `probing N providers…`，直到用户按键或动鼠标）。
    refresh_pending: bool,
    /// 本次探测的目标总数（供 Environment 段与顶栏使用）。
    target_count: usize,
}

/// 取扩展 UI 状态的进程级单例（惰性初始化）。
fn state() -> &'static Mutex<State> {
    /// 扩展 UI 状态的进程级单例。
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(State::default()))
}

/// 取状态锁；锁被毒化时取回内部值。
fn lock() -> MutexGuard<'static, State> {
    state().lock().unwrap_or_else(|e| e.into_inner())
}

/// `doctor` 扩展。
pub struct Doctor;

impl Extension for Doctor {
    /// 扩展名 `doctor`。
    fn name(&self) -> &str {
        EXT
    }

    /// 一句话说明：并发探测各 provider 端点是否可达。
    fn description(&self) -> &str {
        "Probe provider endpoints for network reachability (/doctor, /doctor summary)"
    }

    /// 默认**不启用**：属于 `Dev` / `Creator` 模式下的排查工具，由用户在 `/extension` 面板开启。
    fn default_enabled(&self) -> bool {
        false
    }

    /// 只在 `Dev` / `Creator` 扩展模式下可用。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Dev, ExtensionMode::Creator]
    }

    /// 不提供模型工具（诊断只面向用户）。
    fn tools(&self) -> Vec<ExtensionTool> {
        Vec::new()
    }

    /// 声明 `/doctor` 命令及 `summary` 子命令。
    fn commands(&self) -> Vec<ExtensionCommand> {
        vec![ExtensionCommand {
            name: "doctor".to_string(),
            description: "/doctor <summary> (probe provider endpoints for reachability)"
                .to_string(),
            busy_safe: true, // 只起后台探测任务，不锁 agent
            subcommands: vec![SubcommandDef {
                name: "summary",
                description: "Only show reachable / failed counts plus the failure list",
            }],
        }]
    }

    /// 注册 `/doctor` 的 handler 到斜杠命令表。
    fn on_registered(&self) {
        register_slash_command(EXT, "doctor", command_doctor);
    }

    /// 禁用时收起面板并清状态：禁用后框架不再轮询本扩展。
    fn on_enabled_changed(&self, enabled: bool) {
        if !enabled {
            clear(None);
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

    /// 探测期间或有结果待渲染时重绘：面板要显示 `probing…`，并在结果就绪后换成诊断。
    fn wants_redraw(&self) -> bool {
        let st = lock();
        st.probing || st.refresh_pending
    }

    /// 渲染诊断覆盖层；`id` 不是当前面板时返回 `None`。
    fn overlay_view(&self, id: u64, _cols: u16) -> Option<OverlayView> {
        let mut st = lock();
        if st.overlay_id != Some(id) {
            return None;
        }

        // 内容即将被渲染：这一帧之后不再为此额外请求重绘
        st.refresh_pending = false;

        let lines = current_lines(&st);

        Some(OverlayView {
            title: if st.summary {
                "Doctor · summary".to_string()
            } else {
                "Doctor".to_string()
            },
            header: Vec::new(),
            lines,
            messages: Vec::new(),
            footer: vec![
                (DEF_ARROW_UP_DOWN.to_string(), "scroll".to_string()),
                ("Esc".to_string(), "close".to_string()),
            ],
            input: None,
            editor: None,
            size: OverlaySize::Panel {
                max_rows: MAX_PANEL_ROWS as u16,
            },
            selected: None,
            input_focus: false,
        })
    }

    /// 处理覆盖层事件：只认领 `Closed` 清状态；↑↓ / PageUp / PageDown / Home / End 交给 TUI 默认滚动。
    ///
    /// 扩展**不能**自己 `skip` 行：面板高度 = 内容行数 + 固定行（上限 `max_rows`），
    /// 视窗一变短面板就跟着变矮，看起来像“按 ↓ 面板缩了个子”而不是滚动。
    fn on_overlay_event(&self, id: u64, ev: &OverlayEvent) -> bool {
        match ev {
            OverlayEvent::Closed => clear(Some(id)),
            // Esc 等其余键不消费：交给 TUI 默认语义（滚动 / 关闭，随后收到 Closed）
            _ => false,
        }
    }
}

/// 关闭面板并清状态；`Some(id)` 时只清理匹配的面板。
/// 返回是否确实清理了当前面板。
fn clear(id: Option<u64>) -> bool {
    let mut st = lock();
    if let Some(id) = id
        && st.overlay_id != Some(id)
    {
        return false;
    }

    st.overlay_id = None;
    st.results = None;
    st.skip_reason = None;
    st.probing = false;
    st.refresh_pending = false;
    st.summary = false;
    st.target_count = 0;
    true
}

/// `/doctor` handler：解析子命令 → 后台并发探测 → 打开面板。
fn command_doctor(st: &mut App, raw: &str) -> bool {
    match command_arg(raw).trim() {
        "" => start_probe(false),
        "summary" => start_probe(true),
        other => st.push_msg(
            format!("Unknown /doctor subcommand: {other}. Use `summary` or no argument."),
            MsgLevel::Warning,
        ),
    }
    false
}

/// 打开面板并启动后台探测（离线 / 无 tokio 运行时时只提示不发请求）。
fn start_probe(summary: bool) {
    let targets = probe_targets();
    let target_count = targets.len();
    let offline = runtime::offline();
    let id = next_ui_id();

    {
        let mut st = lock();
        st.overlay_id = Some(id);
        // 离线时不进入 probing：直接落空结果 + 跳过原因
        st.results = if offline { Some(Vec::new()) } else { None };
        st.skip_reason = offline.then(|| "offline mode: network probes skipped".to_string());
        st.summary = summary;
        st.probing = !offline;
        st.refresh_pending = false;
        st.target_count = target_count;
    }
    request_ui(ExtensionUiRequest::ShowOverlay { id });

    if offline {
        return;
    }

    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            handle.spawn(async move {
                let results = run_probes(targets).await;
                finish_probe(id, results, None);
            });
        }
        // 单测等无 tokio 运行时的路径：不发网络请求，直接提示（不 panic）
        Err(_) => finish_probe(
            id,
            Vec::new(),
            Some("no async runtime: network probes skipped".to_string()),
        ),
    }
}

/// 探测结束：面板还开着同一 id 时回填结果，并请求再重绘一帧。
///
/// 丢弃不匹配的结果（用户已关掉面板，或又发起了新一轮探测）。
fn finish_probe(id: u64, results: Vec<ProbeResult>, skip_reason: Option<String>) {
    let mut st = lock();
    if st.overlay_id != Some(id) {
        return;
    }

    st.results = Some(results);
    st.skip_reason = skip_reason;
    st.probing = false;
    st.refresh_pending = true;
}

/// 收集全部探测目标：每个 provider 取第一个 chat 模型，用其 `base_url` 作探测地址。
///
/// 没有 chat 模型、`base_url` 为空或不是 http(s) 绝对地址的 provider 直接跳过。
fn probe_targets() -> Vec<Target> {
    let mut out = Vec::new();
    for provider in model_resolver::catalog_providers() {
        let Some((model_id, _)) = model_resolver::list_models(&provider).into_iter().next() else {
            continue;
        };
        let Ok(entry) = model_resolver::find_model(&provider, &model_id) else {
            continue;
        };
        let base_url = entry.base_url.trim().to_string();
        if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
            continue;
        }
        out.push(Target {
            host: host_of(&base_url),
            provider,
            api: entry.api,
            base_url,
        });
    }
    out
}

/// 从 URL 里解析 `host[:port]`（自己实现，不引入新依赖）。
///
/// 去掉 scheme、userinfo、path/query/fragment；IPv6 保留方括号。解析不出主机时返回空串。
fn host_of(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    if host_port.is_empty() {
        return String::new();
    }

    let (host, port) = if let Some(after) = host_port.strip_prefix('[') {
        // IPv6 字面量：[::1] 或 [::1]:8080
        match after.split_once(']') {
            Some((h, tail)) => (
                format!("[{h}]"),
                tail.strip_prefix(':').unwrap_or("").to_string(),
            ),
            None => (format!("[{after}"), String::new()),
        }
    } else if let Some((h, p)) = host_port.rsplit_once(':') {
        // 只有冒号后全是数字、且主机段不含冒号时才当作端口
        if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) && !h.contains(':') {
            (h.to_string(), p.to_string())
        } else {
            (host_port.to_string(), String::new())
        }
    } else {
        (host_port.to_string(), String::new())
    };

    if port.is_empty() {
        host
    } else {
        format!("{host}:{port}")
    }
}

/// 可达响应的说明文案。
///
/// `2xx` / `401` / `403` / `405` 是常见且预期的结果（鉴权问题、Cloudflare 拦 `HEAD`、
/// 端点不接受 `HEAD`），不加附注；其它状态码显式标 `status not OK`——网络通了但服务端
/// 给的答案不对，是另一类要单独看的问题。
fn reachable_detail(code: u16) -> String {
    match code {
        200..=299 | 401 | 403 | 405 => format!("{code} (reachable, auth not checked)"),
        other => format!("{other} (reachable, status not OK)"),
    }
}

/// 把请求错误压成简短原因：超时 → `timeout`，DNS/连接/TLS 失败 → `connect`/`dns`。
///
/// 不回传 reqwest 的完整错误链（多层 `error sending request ... caused by ...` 太噪音）。
fn classify_error(err: &reqwest::Error) -> String {
    if err.is_timeout() {
        return "timeout".to_string();
    }
    if err.is_connect() {
        let text = err.to_string().to_ascii_lowercase();
        if text.contains("dns") || text.contains("resolve") || text.contains("lookup") {
            return "dns error".to_string();
        }
        return "connect error".to_string();
    }
    if err.is_request() {
        return "request error".to_string();
    }
    "request failed".to_string()
}

/// 探测单个目标（默认 [`PROBE_TIMEOUT`] 超时）。
async fn probe_one(client: &reqwest::Client, target: &Target) -> ProbeResult {
    probe_one_with_timeout(client, target, PROBE_TIMEOUT).await
}

/// 探测核心：发一个 HEAD 请求（不带任何凭据），显式超时便于单测用短超时驱动挂起分支。
async fn probe_one_with_timeout(
    client: &reqwest::Client,
    target: &Target,
    timeout: Duration,
) -> ProbeResult {
    let request = client.head(&target.base_url).timeout(timeout);
    match request.send().await {
        // 有 HTTP 响应即证明链路通（探的是 base URL，"路径无处理器"不算网络故障）
        Ok(response) => ProbeResult::new(target, Outcome::Reachable(response.status().as_u16())),
        Err(err) => ProbeResult::new(target, Outcome::Unreachable(classify_error(&err))),
    }
}

/// 并发探测全部目标（在途请求上限 [`PROBE_CONCURRENCY`]），结果按 provider 名排序。
async fn run_probes(targets: Vec<Target>) -> Vec<ProbeResult> {
    let client = match http::client_builder(None)
        .timeout(PROBE_TIMEOUT)
        .user_agent(default_user_agent())
        .build()
    {
        Ok(client) => client,
        Err(_) => {
            return targets
                .iter()
                .map(|t| ProbeResult::new(t, Outcome::Unreachable("client error".to_string())))
                .collect();
        }
    };

    let mut results: Vec<ProbeResult> =
        futures_util::stream::iter(targets.into_iter().map(|target| {
            let client = client.clone();
            async move { probe_one(&client, &target).await }
        }))
        .buffer_unordered(PROBE_CONCURRENCY)
        .collect()
        .await;

    results.sort_by(|a, b| a.provider.cmp(&b.provider));
    results
}

/// 面板末尾的 environment 段：程序名/版本、agent 目录、探测目标数、代理变量有无。
///
/// `proxy` 是 `(小写变量名, 是否已配置)` 列表（见 [`proxy::env_flags`]），
/// 只渲染 `set` / `unset`，**不回显变量值**（值里可能带凭据）。
fn environment_lines(target_count: usize, proxy: &[(&str, bool)]) -> Vec<DockLine> {
    let proxies = proxy
        .iter()
        .map(|(name, set)| format!("{name}={}", if *set { "set" } else { "unset" }))
        .collect::<Vec<_>>()
        .join(" · ");

    vec![
        vec![DockSpan::new(ACCENT.0, ACCENT.1, "environment")],
        vec![DockSpan::plain(format!(
            "{} {} · agent dir: {}",
            APP_NAME,
            env!("CARGO_PKG_VERSION"),
            settings_manager::agent_dir().display()
        ))],
        vec![DockSpan::plain(format!("probe targets: {target_count}"))],
        vec![DockSpan::new(DIM.0, DIM.1, format!("proxies: {proxies}"))],
    ]
}

/// 面板顶栏行：面板样式不渲染 `title` 字段（只有 Inline 才有标题行），
/// 标题信息只能落在内容首行，做法与 cache-usage 面板一致。
///
/// `summary` 决定是否带上 `summary` 视图名；`provider_count` 是本次探测目标数。
fn title_line(summary: bool, provider_count: usize) -> DockLine {
    let label = if summary {
        format!("Doctor · summary · {provider_count} providers")
    } else {
        format!("Doctor · {provider_count} providers")
    };
    vec![DockSpan::new(ACCENT.0, ACCENT.1, label)]
}

/// 按当前 state 组装完整面板行（首行恒为 [`title_line`]，末段都是 environment）。
fn current_lines(st: &State) -> Vec<DockLine> {
    let title = title_line(st.summary, st.target_count);

    if st.probing {
        return vec![
            title,
            Vec::new(),
            vec![DockSpan::new(DIM.0, DIM.1, "probing…")],
        ];
    }

    let mut lines = vec![title, Vec::new()];
    if let Some(reason) = &st.skip_reason {
        lines.push(vec![DockSpan::new(WARNING.0, WARNING.1, reason.clone())]);
        lines.extend(environment_lines(st.target_count, &proxy::env_flags()));
        return lines;
    }

    match &st.results {
        Some(results) if st.summary => lines.extend(summary_lines(results)),
        Some(results) => lines.extend(detail_lines(results)),
        None => {}
    }
    lines
}

/// 每行一个 provider 的明细视图 + environment 段。
fn detail_lines(results: &[ProbeResult]) -> Vec<DockLine> {
    let mut lines: Vec<DockLine> = Vec::new();

    for result in results {
        let (marker, key, color, detail) = match &result.outcome {
            Outcome::Reachable(code) => ("✓", SUCCESS.0, SUCCESS.1, reachable_detail(*code)),
            Outcome::Unreachable(reason) => ("✗", ERROR.0, ERROR.1, reason.clone()),
        };
        lines.push(vec![
            DockSpan::new(key, color, format!("{marker} ")),
            DockSpan::plain(format!(
                "{} · {} · {} · {}",
                result.provider, result.api, result.host, detail
            )),
        ]);
    }

    if results.is_empty() {
        lines.push(vec![DockSpan::new(DIM.0, DIM.1, "no probe targets")]);
    }

    lines.push(Vec::new());
    lines.extend(environment_lines(results.len(), &proxy::env_flags()));
    lines
}

/// `reachable / failed` 计数 + 失败清单（provider + 原因）+ environment 段。
fn summary_lines(results: &[ProbeResult]) -> Vec<DockLine> {
    let reachable = results
        .iter()
        .filter(|r| matches!(r.outcome, Outcome::Reachable(_)))
        .count();
    let failures: Vec<&ProbeResult> = results
        .iter()
        .filter(|r| matches!(r.outcome, Outcome::Unreachable(_)))
        .collect();

    let mut lines: Vec<DockLine> = vec![vec![DockSpan::new(
        ACCENT.0,
        ACCENT.1,
        format!("{reachable} reachable · {} failed", failures.len()),
    )]];

    if failures.is_empty() {
        lines.push(vec![DockSpan::new(
            SUCCESS.0,
            SUCCESS.1,
            "all providers reachable",
        )]);
    } else {
        lines.push(Vec::new());
        lines.push(vec![DockSpan::new(ACCENT.0, ACCENT.1, "failed")]);
        for result in failures {
            let Outcome::Unreachable(reason) = &result.outcome else {
                continue;
            };
            lines.push(vec![DockSpan::new(
                ERROR.0,
                ERROR.1,
                format!("✗ {} · {reason}", result.provider),
            )]);
        }
    }

    lines.push(Vec::new());
    lines.extend(environment_lines(results.len(), &proxy::env_flags()));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::extensions::OverlayKey;
    use crate::test_support::{AUTH_TEST_LOCK, AgentDirGuard, run_with_timeout};
    use tokio::{
        io::{AsyncReadExt as _, AsyncWriteExt as _},
        net::TcpListener,
    };

    /// 全局状态锁（多个用例共享进程级 state）。
    fn lock_global() -> MutexGuard<'static, ()> {
        AUTH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 清空待处理的 UI 请求队列（避免用例间互相 `take_pending_ui`）。
    fn drain_ui() {
        while crate::core::extensions::take_pending_ui().is_some() {}
    }

    /// 构造一个目标（host 由 base_url 现算）。
    fn target(provider: &str, base_url: &str) -> Target {
        Target {
            provider: provider.to_string(),
            api: "anthropic-messages".to_string(),
            base_url: base_url.to_string(),
            host: host_of(base_url),
        }
    }

    /// 一个可达结果。
    fn reachable(provider: &str, code: u16) -> ProbeResult {
        ProbeResult::new(
            &target(provider, "https://example.test"),
            Outcome::Reachable(code),
        )
    }

    /// 一个不可达结果。
    fn unreachable(provider: &str, reason: &str) -> ProbeResult {
        ProbeResult::new(
            &target(provider, "https://example.test"),
            Outcome::Unreachable(reason.to_string()),
        )
    }

    /// 把面板行拼成纯文本，便于断言。
    fn text(lines: &[DockLine]) -> String {
        lines
            .iter()
            .map(|line| line.iter().map(|s| s.text.as_str()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn host_of_strips_scheme_userinfo_and_keeps_port() {
        assert_eq!(host_of("https://api.anthropic.com"), "api.anthropic.com");
        assert_eq!(
            host_of("https://api.example.com:8443/v1/messages"),
            "api.example.com:8443"
        );
        assert_eq!(
            host_of("https://user:secret@host.example/v1?token=x#frag"),
            "host.example"
        );
        assert_eq!(host_of("http://[::1]:8080/x"), "[::1]:8080");
        assert_eq!(host_of("https://[2001:db8::1]"), "[2001:db8::1]");
        assert_eq!(host_of("not-a-url"), "not-a-url");
    }

    #[test]
    fn reachable_detail_marks_expected_and_unexpected_statuses() {
        assert_eq!(reachable_detail(200), "200 (reachable, auth not checked)");
        assert_eq!(reachable_detail(401), "401 (reachable, auth not checked)");
        assert_eq!(reachable_detail(403), "403 (reachable, auth not checked)");
        assert_eq!(reachable_detail(405), "405 (reachable, auth not checked)");
        // 网络通了但服务端答案不对：仍然算可达，但行里要看得出来
        assert_eq!(reachable_detail(404), "404 (reachable, status not OK)");
        assert_eq!(reachable_detail(500), "500 (reachable, status not OK)");
    }

    #[test]
    fn detail_lines_show_markers_status_and_environment() {
        let results = vec![
            reachable("anthropic", 200),
            unreachable("openai", "timeout"),
        ];
        let joined = text(&detail_lines(&results));
        assert!(joined.contains(
            "✓ anthropic · anthropic-messages · example.test · 200 (reachable, auth not checked)"
        ));
        assert!(joined.contains("✗ openai · anthropic-messages · example.test · timeout"));
        assert!(joined.contains("environment"));
        assert!(joined.contains(&format!("{APP_NAME} {}", env!("CARGO_PKG_VERSION"))));
        assert!(joined.contains("agent dir:"));
        assert!(joined.contains("probe targets: 2"));
        // 代理只显示 set/unset，不回显值
        for name in proxy::ENV_VARS {
            assert!(
                joined.contains(&format!("{name}=set"))
                    || joined.contains(&format!("{name}=unset"))
            );
        }
    }

    #[test]
    fn summary_lines_count_and_list_failures() {
        let results = vec![
            reachable("anthropic", 401),
            reachable("google", 200),
            unreachable("openai", "HTTP 500"),
        ];
        let joined = text(&summary_lines(&results));
        assert!(joined.contains("2 reachable · 1 failed"));
        assert!(joined.contains("failed"));
        assert!(joined.contains("✗ openai · HTTP 500"));
        assert!(joined.contains("environment"));

        let ok = text(&summary_lines(&[reachable("anthropic", 200)]));
        assert!(ok.contains("1 reachable · 0 failed"));
        assert!(ok.contains("all providers reachable"));
    }

    #[test]
    fn environment_lines_render_set_unset_without_values() {
        let lines = environment_lines(
            3,
            &[
                ("http_proxy", true),
                ("https_proxy", false),
                ("all_proxy", false),
                ("no_proxy", false),
            ],
        );
        let joined = text(&lines);
        assert!(joined.contains("environment"));
        assert!(joined.contains("probe targets: 3"));
        assert!(joined.contains("http_proxy=set"));
        assert!(joined.contains("https_proxy=unset"));
        assert!(joined.contains("all_proxy=unset"));
        assert!(joined.contains("no_proxy=unset"));
    }

    #[test]
    fn empty_results_still_show_environment() {
        let joined = text(&detail_lines(&[]));
        assert!(joined.contains("no probe targets"));
        assert!(joined.contains("probe targets: 0"));
        assert!(joined.contains("environment"));
    }

    #[test]
    fn probe_targets_have_valid_base_urls() {
        let targets = probe_targets();
        assert!(!targets.is_empty(), "内置目录应有可探测的 provider");
        for t in &targets {
            assert!(t.base_url.starts_with("http://") || t.base_url.starts_with("https://"));
            assert!(!t.host.is_empty(), "{} 的 host 不应为空", t.provider);
            assert!(!t.api.is_empty());
        }
    }

    #[test]
    fn command_with_unknown_subcommand_prints_usage() {
        let _g = lock_global();
        drain_ui();
        let mut st = App::new();
        assert!(!command_doctor(&mut st, "doctor bogus"), "不应退出 TUI");
        assert!(!st.system_messages.is_empty(), "未知子命令应提示用法");
        assert_eq!(lock().overlay_id, None, "未知子命令不应打开面板");
    }

    #[test]
    fn overlay_claims_own_id_and_leaves_scrolling_to_the_tui() {
        let _g = lock_global();
        let _ad = AgentDirGuard::temp();
        drain_ui();

        let id = next_ui_id();
        {
            let mut st = lock();
            st.overlay_id = Some(id);
            st.results = Some(vec![reachable("anthropic", 200)]);
            st.probing = false;
            st.summary = false;
            st.target_count = 1;
        }

        let ext = Doctor;
        assert_eq!(ext.hooks(), vec![ExtensionHook::Overlay]);
        let view = ext.overlay_view(id, 80).expect("认领自己的 id");
        assert_eq!(view.title, "Doctor");
        assert!(matches!(view.size, OverlaySize::Panel { .. }));
        assert!(ext.overlay_view(id + 1, 80).is_none(), "不认领其它 id");

        // 滚动键一律不消费：面板滚动由 TUI 默认语义做（扩展自己 skip 行会让面板跟着缩）
        let total = {
            let st = lock();
            current_lines(&st).len()
        };
        assert_eq!(view.lines.len(), total, "内容一次给全，裁剪交由覆盖层");
        for key in [
            OverlayKey::Up,
            OverlayKey::Down,
            OverlayKey::PageDown,
            OverlayKey::Home,
            OverlayKey::End,
        ] {
            assert!(!ext.on_overlay_event(id, &OverlayEvent::Key { key, input: None }));
        }

        // Esc 也不消费（交 TUI 默认语义关闭）
        assert!(!ext.on_overlay_event(
            id,
            &OverlayEvent::Key {
                key: OverlayKey::Esc,
                input: None
            }
        ));

        assert!(ext.on_overlay_event(id, &OverlayEvent::Closed));
        assert_eq!(lock().overlay_id, None);
    }

    #[test]
    fn panel_top_line_names_the_view() {
        let results = vec![reachable("anthropic", 200)];
        let mut st = State {
            results: Some(results),
            target_count: 3,
            ..State::default()
        };

        assert_eq!(text(&current_lines(&st)[..1]), "Doctor · 3 providers");

        st.summary = true;
        assert_eq!(
            text(&current_lines(&st)[..1]),
            "Doctor · summary · 3 providers",
            "summary 视图的顶栏要能区分自己是哪一档"
        );
    }

    #[test]
    fn probing_panel_shows_progress_and_wants_redraw() {
        let _g = lock_global();
        drain_ui();
        let id = next_ui_id();
        {
            let mut st = lock();
            st.overlay_id = Some(id);
            st.results = None;
            st.probing = true;
            st.target_count = 5;
        }
        assert!(Doctor.wants_redraw());
        let view = Doctor.overlay_view(id, 80).unwrap();
        let joined = text(&view.lines);
        assert!(joined.contains("Doctor · 5 providers"));
        assert!(joined.contains("probing…"));
        clear(None);
        assert!(!Doctor.wants_redraw());
    }

    #[test]
    fn finished_probe_still_wants_one_frame() {
        let _g = lock_global();
        let id = next_ui_id();
        {
            let mut st = lock();
            st.overlay_id = Some(id);
            st.results = None;
            st.probing = true;
            st.refresh_pending = false;
            st.target_count = 3;
        }
        assert!(Doctor.wants_redraw(), "探测中要重绘");

        // 后台探测完成：probing 归 false，但结果还没渲染过，必须仍要一帧，
        // 否则空闲时 TUI 不会再刷（80ms tick 只轮询 wants_redraw）。
        finish_probe(id, Vec::new(), None);
        assert!(!lock().probing);
        assert!(Doctor.wants_redraw(), "结果就绪后界面还得刷一次");

        assert!(Doctor.overlay_view(id, 80).is_some());
        assert!(!Doctor.wants_redraw(), "渲染过一帧后不再持续重绘");

        // 面板已关掉时探测才回来：结果丢弃，不能留下永久重绘
        assert!(Doctor.on_overlay_event(id, &OverlayEvent::Closed));
        finish_probe(id, Vec::new(), None);
        assert!(lock().results.is_none());
        assert!(!Doctor.wants_redraw());
    }

    #[test]
    fn disabled_extension_clears_state() {
        let _g = lock_global();
        let id = next_ui_id();
        {
            let mut st = lock();
            st.overlay_id = Some(id);
            st.probing = true;
        }
        Doctor.on_enabled_changed(false);
        assert_eq!(lock().overlay_id, None);
        assert!(!Doctor.wants_redraw());
        assert!(Doctor.hooks().is_empty());
    }

    #[test]
    fn command_declares_summary_surface() {
        let cmds = Doctor.commands();
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].name, "doctor");
        assert!(cmds[0].busy_safe);
        assert_eq!(cmds[0].subcommands[0].name, "summary");
        assert_eq!(Doctor.name(), EXT);
    }

    /// 本地假 HTTP 服务器：`hang` 为 true 时读完请求挂着不回；否则返回固定状态码。
    async fn spawn_server(status: u16, hang: bool) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                if hang {
                    // 读完请求就挂着，让客户端超时
                    let mut buf = [0u8; 1024];
                    let _ = socket.read(&mut buf).await;
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    continue;
                }
                let mut buf = [0u8; 1024];
                let _ = socket.read(&mut buf).await;
                let reason = match status {
                    200 => "OK",
                    401 => "Unauthorized",
                    _ => "Error",
                };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        (format!("http://{addr}"), handle)
    }

    /// 不联网：用本地服务器（不读环境代理）驱动 200 / 401 / 挂起 / 连接失败四条分支。
    #[test]
    fn local_probe_classifies_reachable_timeout_and_connect_error() {
        let _g = lock_global();
        run_with_timeout(Duration::from_secs(30), || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let client = reqwest::Client::new();

                let (url, handle) = spawn_server(200, false).await;
                let result = probe_one(&client, &target("local", &url)).await;
                assert_eq!(result.outcome, Outcome::Reachable(200));
                handle.abort();

                let (url, handle) = spawn_server(401, false).await;
                let result = probe_one(&client, &target("local", &url)).await;
                assert_eq!(
                    result.outcome,
                    Outcome::Reachable(401),
                    "401 说明端点活着，应判可达"
                );
                handle.abort();

                // 404：探的是 base URL，"这个路径没有处理器"同样证明链路通
                let (url, handle) = spawn_server(404, false).await;
                let result = probe_one(&client, &target("local", &url)).await;
                assert_eq!(
                    result.outcome,
                    Outcome::Reachable(404),
                    "有 HTTP 响应就说明网络通（状态码由行文本区分）"
                );
                assert_eq!(reachable_detail(404), "404 (reachable, status not OK)");
                handle.abort();

                let (url, handle) = spawn_server(200, true).await;
                let result = probe_one_with_timeout(
                    &client,
                    &target("local", &url),
                    Duration::from_millis(300),
                )
                .await;
                assert_eq!(result.outcome, Outcome::Unreachable("timeout".to_string()));
                handle.abort();

                // 绑一个端口后立刻释放，再连它 → 连接被拒（DNS/连接类失败）
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                drop(listener);
                let result = probe_one(&client, &target("local", &format!("http://{addr}"))).await;
                assert_eq!(
                    result.outcome,
                    Outcome::Unreachable("connect error".to_string())
                );
            });
        });
    }
}
