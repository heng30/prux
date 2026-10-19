//! `usage` 扩展：逐个已连接 provider 展示**配额 / 限额用量**与重置时间（`/usage`）。
//!
//! 与 `cache-usage` 的分工：那个统计**本机会话历史**的 prompt cache 命中率（读会话文件），
//! 这个查**provider 侧**的配额（发请求）。前者能离线工作，后者必须联网且有凭据。
//!
//! # 数据源
//!
//! 只有少数 provider 暴露配额端点，全部列在 [`fetch::SUPPORTED`]：
//!
//! - **订阅型**（OAuth 登录）：Claude 的 `/api/oauth/usage`（5 小时 / 7 天 / 模型周窗口）、
//!   ChatGPT 的 `backend-api/wham/usage`（5 小时 / 周 + 按模型窗口）、Copilot 的
//!   `copilot_internal/v2/token`（各功能已用/上限）、OpenRouter 的 `credits` + `key`；
//! - **API key**：Kimi for Coding 的 `coding/v1/usages`、Z.AI Coding Plan 的
//!   `monitor/usage/quota/limit`、DeepSeek 与 Moonshot 的余额端点。
//!
//! 其余已配置的 provider 没有可查的端点，面板末行会点名它们，而不是静默不显示。
//!
//! # 两种视角
//!
//! 面板里每个 provider 先画**配额条**（窗口名 + 24 格条 + 百分比 + 重置倒计时），
//! 再列**附加信息**（余额、套餐、键状态、重置券张数）。没有窗口的 provider（余额型）
//! 只出附加信息。
//!
//! # 边界
//!
//! - `overlay_view` 每帧调用、必须同步无 I/O：数据在 `/usage` 执行时异步拉好并缓存在扩展
//!   state 里，面板只读缓存；报告有 TTL，过期后再次 `/usage` 会重新拉取。
//! - `--offline` / `PRUX_OFFLINE` 下不发任何请求，面板只提示离线。
//! - 配额端点未文档化、随 provider 变动：单个 provider 失败只影响它自己那一行。

mod fetch;

use crate::{
    core::{
        extensions::{
            DockLine, DockSpan, Extension, ExtensionCommand, ExtensionHook, ExtensionMode,
            ExtensionTool, ExtensionUiRequest, ForkProjectInfo, OverlayEvent, OverlaySize,
            OverlayView, next_ui_id, request_ui,
        },
        runtime,
    },
    extensions::{
        ACCENT, DIM, ERROR, EXTENSION_FACTORIES, ExtensionFactory, MUTED, PRIORITY_USAGE, SUCCESS,
        command_arg, util::truncate_chars,
    },
    modes::interactive::{
        app::{App, MsgLevel},
        handlers::register_slash_command,
    },
    utils::{
        glyphs::{DEF_ARROW_UP_DOWN, DEF_BAR_EMPTY, DEF_BAR_FULL},
        time::{now_ms, rel_time},
    },
};
use fetch::{ProviderReport, QuotaWindow};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

/// 扩展名。
const EXT: &str = "usage";

/// 配额条的格数。
const BAR_WIDTH: usize = 24;

/// 覆盖面板可见行上限。
const MAX_PANEL_ROWS: usize = 24;

/// 报告的新鲜期：超过这么久再次 `/usage` 会重新拉取。
const REPORT_TTL_MS: u64 = 120_000;

/// 自声明工厂：linkme 分布式切片。
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static USAGE_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_USAGE,
    make: || -> Arc<dyn Extension> { Arc::new(Usage) },
};

/// 扩展 UI 状态（进程级单例）。
#[derive(Default)]
struct State {
    /// 当前覆盖层 id（None = 未显示）。
    overlay_id: Option<u64>,
    /// 已拉好的报告（None = 还在拉取或尚未请求过）。
    reports: Option<Vec<ProviderReport>>,
    /// 已配置但没有配额端点的 provider（面板末行）。
    unqueried: Vec<String>,
    /// 拉取进行中（驱动面板文案与重绘）。
    loading: bool,
    /// 报告刚就绪、还没渲染过：置位后至少再要一帧重绘。
    ///
    /// 空闲时 TUI 只在 80ms tick 里轮询 [`Extension::wants_redraw`]，拉取若碰巧落在两次
    /// tick 之间完成，`loading` 已经归 false，没有这个标志就再也等不到重绘
    /// （界面停在 `loading…`，直到用户按键或动鼠标）。
    refresh_pending: bool,
    /// 报告的拉取时刻（Unix 毫秒），用于 TTL 判定。
    fetched_at: u64,
    /// 跳过网络的说明（离线）；None = 正常拉取。
    skip_reason: Option<String>,
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

/// `usage` 扩展。
pub struct Usage;

impl Extension for Usage {
    /// 扩展名 `usage`。
    fn name(&self) -> &str {
        EXT
    }

    /// 一句话说明：逐个 provider 显示配额用量与重置时间。
    fn description(&self) -> &str {
        "Show provider quota / rate-limit usage and reset times (/usage)"
    }

    /// 默认**不启用**：属于 `Dev` / `Creator` 模式下的运维面板，由用户在 `/extension` 面板开启。
    fn default_enabled(&self) -> bool {
        false
    }

    /// 只在 `Dev` / `Creator` 扩展模式下可用。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Dev, ExtensionMode::Creator]
    }

    /// 来源声明：本扩展按 jcode 0.94.0 的 `/usage` 覆盖面板重写。
    fn fork_project(&self) -> Option<ForkProjectInfo> {
        Some(ForkProjectInfo {
            plugin_name: "jcode".to_string(),
            plugin_version: "0.94.0".to_string(),
            url: "https://github.com/1jehuang/jcode".to_string(),
        })
    }

    /// 不提供模型工具（配额面板只面向用户）。
    fn tools(&self) -> Vec<ExtensionTool> {
        Vec::new()
    }

    /// 声明 `/usage` 命令。
    fn commands(&self) -> Vec<ExtensionCommand> {
        vec![ExtensionCommand {
            name: "usage".to_string(),
            description: "/usage (provider quota / rate-limit usage and reset times)".to_string(),
            busy_safe: true, // 只起后台拉取任务，不锁 agent
            subcommands: Vec::new(),
        }]
    }

    /// 注册 `/usage` 的 handler 到斜杠命令表。
    fn on_registered(&self) {
        register_slash_command(EXT, "usage", command_usage);
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

    /// 拉取期间或有结果待渲染时重绘：面板要显示 `loading…`，并在结果就绪后换成报告。
    fn wants_redraw(&self) -> bool {
        let st = lock();
        st.loading || st.refresh_pending
    }

    /// 渲染配额覆盖层；`id` 不是当前面板时返回 `None`。
    fn overlay_view(&self, id: u64, _cols: u16) -> Option<OverlayView> {
        let mut st = lock();
        if st.overlay_id != Some(id) {
            return None;
        }

        // 内容即将被渲染：这一帧之后不再为此额外请求重绘
        st.refresh_pending = false;

        let lines = match (&st.reports, &st.skip_reason) {
            (_, Some(reason)) => vec![vec![DockSpan::new(DIM.0, DIM.1, reason.clone())]],
            (None, None) => vec![vec![DockSpan::new(DIM.0, DIM.1, "loading quotas…")]],
            (Some(reports), None) => report_lines(reports, &st.unqueried, st.fetched_at),
        };

        Some(OverlayView {
            title: "Usage".to_string(),
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

    /// 处理覆盖层事件：只认领 `Closed` 清状态；滚动键交给 TUI 默认语义。
    ///
    /// 扩展**不能**自己 `skip` 行：面板高度 = 内容行数 + 固定行（上限 `max_rows`），
    /// 视窗一变短面板就跟着变矮，看起来像“按 ↓ 面板缩了个子”而不是滚动。
    fn on_overlay_event(&self, id: u64, ev: &OverlayEvent) -> bool {
        match ev {
            OverlayEvent::Closed => clear(Some(id)),
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
    st.reports = None;
    st.unqueried.clear();
    st.loading = false;
    st.refresh_pending = false;
    st.fetched_at = 0;
    st.skip_reason = None;
    true
}

/// `/usage` handler：无参数时打开面板（缓存未过期就复用），有参数时报错。
fn command_usage(st: &mut App, raw: &str) -> bool {
    let arg = command_arg(raw).trim();
    if !arg.is_empty() {
        st.push_msg(
            format!("Usage: /usage (unexpected argument: {arg})"),
            MsgLevel::Warning,
        );
        return false;
    }

    start_report();
    false
}

/// 打开面板并按需拉取：缓存还新鲜就直接用，否则后台重新拉。
fn start_report() {
    let id = next_ui_id();
    let offline = runtime::offline();
    let now = now_ms();

    let (needs_fetch, cached) = {
        let st = lock();
        let fresh = st
            .reports
            .as_ref()
            .is_some_and(|_| now.saturating_sub(st.fetched_at) < REPORT_TTL_MS);
        (st.reports.is_none() || !fresh, st.reports.clone())
    };

    {
        let mut st = lock();
        st.overlay_id = Some(id);
        st.reports = cached;
        st.skip_reason = offline.then(|| "offline mode: quota requests skipped".to_string());
        st.loading = !offline && needs_fetch;
        st.refresh_pending = false;
        if offline {
            st.fetched_at = 0;
            st.unqueried.clear();
        }
    }
    request_ui(ExtensionUiRequest::ShowOverlay { id });

    if offline || !needs_fetch {
        return;
    }

    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            handle.spawn(async move {
                let reports = fetch::fetch_all().await;
                let unqueried = fetch::unqueried_providers();
                finish_report(id, reports, unqueried);
            });
        }
        Err(_) => {
            // 没有 tokio 运行时（单测 / 极简环境）：不发请求，直接给出说明
            finish_report(
                id,
                Vec::new(),
                vec!["no async runtime: quota requests skipped".to_string()],
            );
        }
    }
}

/// 拉取完成：面板还开着同一 id 时回填结果，并请求再重绘一帧。
///
/// 丢弃不匹配的结果（用户已关掉面板，或又发起了新一轮）。
fn finish_report(id: u64, reports: Vec<ProviderReport>, unqueried: Vec<String>) {
    let mut st = lock();
    if st.overlay_id != Some(id) {
        return;
    }

    st.reports = Some(reports);
    st.unqueried = unqueried;
    st.loading = false;
    st.refresh_pending = true;
    st.fetched_at = now_ms();
}

/// 报告面板的全部行（渲染与滚动上限共用同一份）。
fn report_lines(
    reports: &[ProviderReport],
    unqueried: &[String],
    fetched_at: u64,
) -> Vec<DockLine> {
    let mut lines: Vec<DockLine> = Vec::new();

    // 面板样式不渲染 `title`（只有 Inline 才有标题行），标题信息落在内容首行
    lines.push(vec![DockSpan::new(
        ACCENT.0,
        ACCENT.1,
        format!(
            "Usage · {} providers · fetched {}",
            reports.len(),
            rel_time(Some(fetched_at))
        ),
    )]);

    if reports.is_empty() {
        lines.push(Vec::new());
        lines.push(vec![DockSpan::new(
            DIM.0,
            DIM.1,
            "no configured provider exposes a quota endpoint",
        )]);
    }

    for report in reports {
        lines.push(Vec::new());
        lines.extend(provider_lines(report));
    }

    if !unqueried.is_empty() {
        lines.push(Vec::new());
        lines.push(vec![DockSpan::new(
            MUTED.0,
            MUTED.1,
            format!("no quota endpoint: {}", unqueried.join(", ")),
        )]);
    }

    lines
}

/// 单个 provider 的段落：标题行 + 各窗口 + 附加信息。
fn provider_lines(report: &ProviderReport) -> Vec<DockLine> {
    let mut lines: Vec<DockLine> = Vec::new();

    let status_span = match (&report.error, report.windows.is_empty()) {
        (Some(_), _) => DockSpan::new(ERROR.0, ERROR.1, " error"),
        (None, true) => DockSpan::new(MUTED.0, MUTED.1, " (balance only)"),
        (None, false) => DockSpan::new(SUCCESS.0, SUCCESS.1, ""),
    };
    lines.push(vec![
        DockSpan::new(ACCENT.0, ACCENT.1, &report.label),
        DockSpan::new(MUTED.0, MUTED.1, format!(" · {}", report.provider)),
        status_span,
    ]);

    if let Some(error) = &report.error {
        lines.push(vec![DockSpan::new(ERROR.0, ERROR.1, format!("  {error}"))]);
    }

    for window in &report.windows {
        lines.push(window_line(window));
    }

    for (key, value) in &report.extra {
        lines.push(vec![DockSpan::new(
            MUTED.0,
            MUTED.1,
            format!("  {key}: {value}"),
        )]);
    }

    lines
}

/// 一个配额窗口：名称 + 条形图 + 百分比 + 重置倒计时。
fn window_line(window: &QuotaWindow) -> DockLine {
    const NAME_WIDTH: usize = 20;
    let name = truncate_chars(&window.name, NAME_WIDTH);
    let percent = window.percent as f64;
    let mut spans = vec![
        DockSpan::plain(format!("  {name:<NAME_WIDTH$} ")),
        DockSpan::new(ACCENT.0, ACCENT.1, percent_bar(percent, BAR_WIDTH)),
        DockSpan::new(MUTED.0, MUTED.1, format!(" {percent:5.1}%")),
    ];
    if let Some(resets_at) = window.resets_at {
        spans.push(DockSpan::new(
            DIM.0,
            DIM.1,
            format!("  resets {}", rel_time(Some(resets_at))),
        ));
    }
    spans
}

/// 百分比条形图：`width` 格里按百分比填充实心格（四舍五入）。
fn percent_bar(percent: f64, width: usize) -> String {
    let ratio = (percent / 100.0).clamp(0.0, 1.0);
    let filled = ((ratio * width as f64).round() as usize).min(width);
    let mut out = String::with_capacity(width);
    for index in 0..width {
        out.push(if index < filled {
            DEF_BAR_FULL
        } else {
            DEF_BAR_EMPTY
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::fetch::ProviderReport;
    use super::*;
    use crate::{
        core::auth,
        extensions::WARNING,
        test_support::{AUTH_TEST_LOCK, AgentDirGuard},
        utils::time::now_ms,
    };

    fn window_level(percent: f32) -> (&'static str, &'static str) {
        if percent >= 90.0 {
            ERROR
        } else if percent >= 70.0 {
            WARNING
        } else {
            SUCCESS
        }
    }

    /// 全局状态锁（多个用例共享进程级 state 与 auth.json）。
    fn lock_global() -> MutexGuard<'static, ()> {
        AUTH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 清空待处理的 UI 请求队列（避免用例间互相 `take_pending_ui`）。
    fn drain_ui() {
        while crate::core::extensions::take_pending_ui().is_some() {}
    }

    /// 把面板行拼成纯文本，便于断言。
    fn text(lines: &[DockLine]) -> String {
        lines
            .iter()
            .map(|line| line.iter().map(|s| s.text.as_str()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    // ─── 纯函数：百分比与时间 ──────────────────────────────────────────────

    #[test]
    fn percent_bar_rounds_and_clamps() {
        assert_eq!(percent_bar(0.0, 4), "░░░░");
        assert_eq!(percent_bar(50.0, 4), "██░░");
        assert_eq!(percent_bar(100.0, 4), "████");
        assert_eq!(percent_bar(150.0, 4), "████");
    }

    #[test]
    fn report_lines_render_windows_extras_and_unqueried() {
        let report = ProviderReport {
            provider: "anthropic".to_string(),
            label: "Anthropic".to_string(),
            windows: vec![fetch::QuotaWindow {
                name: "5-hour".to_string(),
                percent: 42.0,
                resets_at: None,
            }],
            extra: vec![("Extra usage".to_string(), "enabled".to_string())],
            error: None,
        };
        let lines = report_lines(&[report], &["groq".to_string()], now_ms());
        let plain = text(&lines);
        assert!(plain.contains("Usage · 1 providers"), "{plain}");
        assert!(plain.contains("Anthropic"), "{plain}");
        assert!(plain.contains("5-hour"), "{plain}");
        assert!(plain.contains("42.0%"), "{plain}");
        assert!(plain.contains("Extra usage: enabled"), "{plain}");
        assert!(plain.contains("no quota endpoint: groq"), "{plain}");
    }

    #[test]
    fn report_lines_show_errors_and_empty_state() {
        let report = ProviderReport::failed("openai", "HTTP 401: nope");
        let plain = text(&report_lines(&[report], &[], now_ms()));
        assert!(plain.contains("OpenAI"), "{plain}");
        assert!(plain.contains("HTTP 401: nope"), "{plain}");
        assert!(plain.contains("error"), "{plain}");

        let plain = text(&report_lines(&[], &[], now_ms()));
        assert!(
            plain.contains("no configured provider exposes a quota endpoint"),
            "{plain}"
        );
    }

    #[test]
    fn window_level_flags_high_usage() {
        assert_eq!(window_level(10.0), SUCCESS);
        assert_eq!(window_level(75.0), WARNING);
        assert_eq!(window_level(95.0), ERROR);
    }

    // ─── 扩展面 ───────────────────────────────────────────────────────────

    #[test]
    fn extension_declares_command_and_defaults_off() {
        assert_eq!(Usage.name(), EXT);
        assert!(!Usage.default_enabled(), "usage 应默认不启用");
        assert_eq!(
            Usage.modes(),
            vec![ExtensionMode::Dev, ExtensionMode::Creator]
        );
        assert!(Usage.tools().is_empty());

        let commands = Usage.commands();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].name, "usage");
        assert!(commands[0].busy_safe);
        assert!(commands[0].subcommands.is_empty());
    }

    #[test]
    fn fork_project_reports_jcode_source() {
        let info = Usage.fork_project().expect("usage 应声明 fork 来源");
        assert_eq!(info.plugin_name, "jcode");
        assert_eq!(info.plugin_version, "0.94.0");
        assert_eq!(info.url, "https://github.com/1jehuang/jcode");
    }

    #[test]
    fn command_rejects_extra_arguments() {
        let _g = lock_global();
        let _ad = AgentDirGuard::temp();
        drain_ui();

        let mut app = App::new();
        assert!(!command_usage(&mut app, "usage bogus"));
        assert!(!app.system_messages.is_empty(), "多余参数应提示用法");
        assert_eq!(lock().overlay_id, None, "报错时不应打开面板");
    }

    #[test]
    fn offline_mode_skips_requests_and_clears_state() {
        let _g = lock_global();
        let _ad = AgentDirGuard::temp();
        drain_ui();
        crate::core::runtime::set_offline(true);

        let mut app = App::new();
        assert!(!command_usage(&mut app, "usage"));
        assert!(app.system_messages.is_empty());

        let st = lock();
        assert!(st.overlay_id.is_some(), "离线也应打开面板");
        assert!(!st.loading, "离线不进入 loading");
        assert!(
            st.skip_reason
                .as_deref()
                .is_some_and(|r| r.contains("offline")),
            "{:?}",
            st.skip_reason
        );
        drop(st);

        // 面板内容就是那一行离线说明（先取出 id，避免临时守卫活到语句结束造成自锁）
        let id = lock().overlay_id.expect("离线也应打开面板");
        let view = Usage.overlay_view(id, 80).expect("当前面板应有内容");
        let plain = text(&view.lines);
        assert!(plain.contains("offline mode"), "{plain}");

        Usage.on_enabled_changed(false);
        assert_eq!(lock().overlay_id, None);
        crate::core::runtime::set_offline(false);
    }

    #[test]
    fn finish_report_ignores_stale_panels() {
        let _g = lock_global();
        let _ad = AgentDirGuard::temp();
        clear(None);

        let id = next_ui_id();
        {
            let mut st = lock();
            st.overlay_id = Some(id);
            st.loading = true;
        }
        finish_report(id, Vec::new(), Vec::new());
        assert!(lock().reports.is_some());
        assert!(!lock().loading);
        assert!(lock().refresh_pending);

        // 换了一个面板 id 后旧结果必须被丢弃
        let stale = id + 1;
        finish_report(stale, Vec::new(), vec!["x".to_string()]);
        assert!(lock().unqueried.is_empty());

        Usage.on_enabled_changed(false);
    }

    /// 有报告时面板画窗口行，`Closed` 事件清空状态。
    #[test]
    fn overlay_renders_report_and_closes() {
        let _g = lock_global();
        let _ad = AgentDirGuard::temp();
        drain_ui();
        clear(None);

        let id = next_ui_id();
        {
            let mut st = lock();
            st.overlay_id = Some(id);
            st.reports = Some(vec![ProviderReport {
                provider: "anthropic".to_string(),
                label: "Anthropic".to_string(),
                windows: vec![fetch::QuotaWindow {
                    name: "5-hour".to_string(),
                    percent: 42.0,
                    resets_at: None,
                }],
                extra: Vec::new(),
                error: None,
            }]);
            st.fetched_at = now_ms();
        }

        let view = Usage.overlay_view(id, 80).expect("当前面板应有内容");
        let plain = text(&view.lines);
        assert!(plain.contains("5-hour"), "{plain}");
        assert!(plain.contains("42.0%"), "{plain}");

        assert!(Usage.on_overlay_event(id, &OverlayEvent::Closed));
        assert_eq!(lock().overlay_id, None);
        assert!(lock().reports.is_none());
    }

    /// 已配置但没有配额端点的 provider 单独成行，不进报告。
    #[test]
    fn configured_providers_without_endpoint_are_listed_separately() {
        let _g = lock_global();
        let _ad = AgentDirGuard::temp();
        let _ek = crate::test_support::env_key_lock();

        auth::write_auth_key("groq", "gsk-test").unwrap();
        let unqueried = fetch::unqueried_providers();
        assert!(unqueried.contains(&"groq".to_string()), "{unqueried:?}");
        assert!(
            !unqueried.iter().any(|id| id == "anthropic"),
            "有端点的 provider 不应出现在未查询清单里"
        );
    }
}
