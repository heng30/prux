//! `cache-usage` 扩展：统计 prompt cache 命中率（当前会话 / 全部会话 / 指定 provider）。
//!
//! 数据源就是 prux 自己的会话文件（`<agent_dir>/sessions/<编码目录>/*.jsonl`）：
//! 每条 assistant 消息，以及 compaction / branch_summary 条目，都带 `Usage`。
//! 命中率口径与 [`crate::extensions::footer`] 底栏一致：
//! `cache_read / (input + cache_read + cache_write)`。
//!
//! 命令 `/cache-usage [all] [<provider>]`，三种范围同级：
//! - 空参：**当前会话**；
//! - `all`：**全部已存会话**（各工程合并，按修改时间取最新 [`MAX_SCAN_FILES`] 份），报告所有 provider；
//! - `<provider>`：同样读全部已存会话，但只保留该 provider 的样本——provider 是跨会话概念，
//!   只在当前会话里找它基本等于无参统计。
//!
//! 结果画在覆盖面板里：总量条形图 + 命中率趋势 sparkline + 各 provider / 各会话分列。
//! 扫描在 `spawn_blocking` 里做（`all` 可能要读上百份会话文件），期间面板显示"scanning…"
//! 并由 [`Extension::wants_redraw`] 驱动重绘。

use crate::{
    core::{
        extensions::{
            DockLine, DockSpan, Extension, ExtensionCommand, ExtensionHook, ExtensionMode,
            ExtensionTool, ExtensionUiRequest, OverlayEvent, OverlaySize, OverlayView,
            SubcommandDef, next_ui_id, request_ui,
        },
        provider::{AgentMessage, Usage},
        session_manager::{self, OpenSessionResult, Session},
        session_v4::Entry,
        settings_manager,
    },
    extensions::{
        ACCENT, DIM, EXTENSION_FACTORIES, ExtensionFactory, MUTED, PRIORITY_CACHE_USAGE,
        command_arg,
    },
    modes::interactive::{
        app::{App, MsgLevel},
        handlers::register_slash_command,
    },
    utils::glyphs::{DEF_ARROW_UP_DOWN, DEF_BAR_EMPTY, DEF_BAR_FULL, DEF_SPARK_BLOCKS},
};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, OnceLock},
    time::SystemTime,
};

/// 扩展名。
const EXT: &str = "cache-usage";

/// 条形图宽度（格）。
const BAR_WIDTH: usize = 24;

/// 趋势 sparkline 的列数。
const TREND_COLS: usize = 32;

/// 覆盖面板可见行上限。
const MAX_PANEL_ROWS: usize = 20;

/// `all` 模式下最多扫描的会话文件数（按修改时间取最新一批）。
const MAX_SCAN_FILES: usize = 200;

/// 报告里最多列出的会话数（按 token 总量取前几名）。
const MAX_SESSION_ROWS: usize = 12;

/// 自声明工厂：linkme 分布式切片。
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static CACHE_USAGE_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_CACHE_USAGE,
    make: || -> Arc<dyn Extension> { Arc::new(CacheUsage) },
};

/// 一次带用量的请求：一个 assistant 消息，或一次压缩 / 分支摘要。
#[derive(Debug, Clone)]
struct Sample {
    /// 产生用量的 provider；消息没写时回退到该会话最近一次 `ModelChange`。
    provider: String,
    /// 未命中缓存的输入 token。
    input: u64,
    /// 命中缓存的输入 token。
    read: u64,
    /// 写入缓存的 token。
    write: u64,
    /// 本次请求的费用（美元）。
    cost: f64,
    /// 条目时间戳（Unix 毫秒），用于趋势排序。
    timestamp: i64,
}

/// 一组样本的 token 汇总。
#[derive(Debug, Default, Clone, Copy)]
struct Rate {
    /// 未命中缓存的输入 token 合计。
    input: u64,
    /// 命中缓存的输入 token 合计。
    read: u64,
    /// 写入缓存的 token 合计。
    write: u64,
}

impl Rate {
    /// 累加一个样本的 token 数。
    fn add(&mut self, sample: &Sample) {
        self.input += sample.input;
        self.read += sample.read;
        self.write += sample.write;
    }

    /// 输入侧的 token 总量（命中率的分母）。
    fn total_input(&self) -> u64 {
        self.input + self.read + self.write
    }

    /// 命中率百分比；没有输入 token 时为 0。
    fn percent(&self) -> f64 {
        let total = self.total_input();
        if total == 0 {
            0.0
        } else {
            self.read as f64 / total as f64 * 100.0
        }
    }
}

/// 一个聚合分组（总量 / 某 provider / 某会话）。
#[derive(Debug, Default, Clone)]
struct Group {
    /// 该组的 token 汇总。
    rate: Rate,
    /// 该组的费用合计（美元）。
    cost: f64,
    /// 该组的响应条数。
    responses: usize,
}

impl Group {
    /// 并入一个样本（调用方负责过滤）。
    fn add(&mut self, sample: &Sample) {
        self.rate.add(sample);
        self.cost += sample.cost;
        self.responses += 1;
    }
}

/// 扫描结果。
#[derive(Debug, Clone)]
struct Report {
    /// 顶部一行：统计范围说明。
    scope: String,
    /// 全部样本的总量（已应用 provider 过滤）。
    total: Group,
    /// 各 provider 的分组（按 token 总量降序）。
    providers: Vec<(String, Group)>,
    /// 各会话的分组（按 token 总量降序，最多 [`MAX_SESSION_ROWS`] 条）。
    sessions: Vec<(String, Group)>,
    /// 按时间排序的每样本命中率百分比（趋势图数据）。
    trend: Vec<f64>,
}

/// 命令参数：`[all] [<provider>]`，顺序无关。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Args {
    /// 是否扫描全部会话（否则只看当前会话）。
    all: bool,
    /// 只看该 provider 的样本；None = 不过滤。
    provider: Option<String>,
}

/// 扩展 UI 状态（进程级单例）。
#[derive(Default)]
struct State {
    /// 当前覆盖层 id（None = 未显示）。
    overlay_id: Option<u64>,
    /// 已算好的报告（None = 还在扫描或未统计）。
    report: Option<Report>,
    /// 是否正在扫描（驱动面板文案与重绘）。
    scanning: bool,
    /// 报告刚就绪、还没渲染过：置位后至少再要一帧重绘，面板认领内容时清除。
    ///
    /// 空闲时 TUI 只在 80ms tick 里轮询 [`Extension::wants_redraw`]，扫描若碰巧
    /// 落在两次 tick 之间完成，`scanning` 已经归 false，没有这个标志就再也等不到重绘
    /// （界面停在 `scanning sessions…`，直到用户按键或动鼠标）。
    refresh_pending: bool,
    /// 当前会话文件路径（由 `on_session_start` / `on_session_switched` 更新）。
    session_file: Option<PathBuf>,
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

/// `cache-usage` 扩展。
pub struct CacheUsage;

impl Extension for CacheUsage {
    /// 扩展名 `cache-usage`。
    fn name(&self) -> &str {
        EXT
    }

    /// 一句话说明：把会话的 prompt cache 命中率按会话 / provider 汇总。
    fn description(&self) -> &str {
        "Report prompt cache hit rates from saved sessions (/cache-usage [all] [provider])"
    }

    /// 默认**不启用**：属于 `Dev` / `Creator` 模式下的排查工具，由用户在 `/extension` 面板开启。
    fn default_enabled(&self) -> bool {
        false
    }

    /// 只在 `Dev` / `Creator` 扩展模式下可用。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Dev, ExtensionMode::Creator]
    }

    /// 不提供模型工具（统计只面向用户）。
    fn tools(&self) -> Vec<ExtensionTool> {
        Vec::new()
    }

    /// 声明 `/cache-usage` 命令及 `all` 子命令。
    fn commands(&self) -> Vec<ExtensionCommand> {
        vec![ExtensionCommand {
            name: "cache-usage".to_string(),
            description: "/cache-usage <all> <provider> (cache hit rate: current session, all sessions, or one provider)"
                .to_string(),
            busy_safe: true, // 只读会话文件与自身状态，不锁 agent
            subcommands: vec![SubcommandDef {
                name: "all",
                description: "Scan every saved session instead of only the current one",
            }],
        }]
    }

    /// 注册 `/cache-usage` 的 handler 到斜杠命令表。
    fn on_registered(&self) {
        register_slash_command(EXT, "cache-usage", command_cache_usage);
    }

    /// 启动时记住当前会话文件（空参统计就查它）。
    fn on_session_start(&self, _cwd: &str, _messages: &[AgentMessage], session: Option<&Session>) {
        remember_session(session.and_then(|s| s.session_file.clone()));
    }

    /// 会话切换后改记新会话文件。
    fn on_session_switched(&self, session_path: Option<&str>, _messages: &[AgentMessage]) {
        remember_session(session_path.map(PathBuf::from));
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

    /// 扫描期间或有结果待渲染时重绘：面板要显示 "scanning…"，并在结果就绪后换成报告。
    fn wants_redraw(&self) -> bool {
        let st = lock();
        st.scanning || st.refresh_pending
    }

    /// 渲染报告覆盖层。
    ///
    /// `id` 不是当前面板时返回 `None`。
    fn overlay_view(&self, id: u64, _cols: u16) -> Option<OverlayView> {
        let mut st = lock();
        if st.overlay_id != Some(id) {
            return None;
        }

        // 内容即将被渲染：这一帧之后不再为此额外请求重绘
        st.refresh_pending = false;

        let Some(report) = st.report.clone() else {
            return Some(OverlayView {
                title: "Cache usage".to_string(),
                header: Vec::new(),
                lines: vec![vec![DockSpan::new(DIM.0, DIM.1, "scanning sessions…")]],
                messages: Vec::new(),
                footer: vec![("Esc".to_string(), "close".to_string())],
                input: None,
                editor: None,
                size: OverlaySize::Panel {
                    max_rows: MAX_PANEL_ROWS as u16,
                },
                selected: None,
                input_focus: false,
            });
        };

        let lines = report_lines(&report);

        Some(OverlayView {
            title: format!("Cache usage · {}", report.scope),
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

    /// 处理覆盖层事件：只认领 `Closed` 清状态；↑↓/PageUp/PageDown/Home/End 交给 TUI 默认滚动。
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

/// 记住当前会话文件路径（None 时清空）。
fn remember_session(path: Option<PathBuf>) {
    let mut st = lock();
    st.session_file = path;
    st.report = None;
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
    st.report = None;
    st.scanning = false;
    st.refresh_pending = false;
    true
}

/// 解析命令参数：`[all] [<provider>]`。
///
/// 多于一个非 `all` 的 token 时报错（消息可直接展示给用户）。
fn parse_args(raw: &str) -> Result<Args, String> {
    let mut args = Args::default();
    for token in raw.split_whitespace() {
        if token == "all" {
            args.all = true;
        } else if args.provider.is_none() {
            args.provider = Some(token.to_string());
        } else {
            return Err(format!(
                "Usage: /cache-usage [all] [<provider>] (unexpected argument: {token})"
            ));
        }
    }
    Ok(args)
}

/// `/cache-usage` handler：解析参数 → 收集会话文件 → 后台扫描 → 打开面板。
fn command_cache_usage(st: &mut App, raw: &str) -> bool {
    let args = match parse_args(command_arg(raw)) {
        Ok(args) => args,
        Err(msg) => {
            st.push_msg(msg, MsgLevel::Warning);
            return false;
        }
    };

    let files = match collect_files(&args) {
        Ok(files) if files.is_empty() => {
            st.push_msg("No saved sessions to scan yet.".to_string(), MsgLevel::Info);
            return false;
        }
        Ok(files) => files,
        Err(msg) => {
            st.push_msg(msg, MsgLevel::Warning);
            return false;
        }
    };

    start_scan(args, files);
    false
}

/// 收集本次要扫描的会话文件（`(显示名, 路径)`）。
///
/// 空参只看当前会话；`all`（全部会话、所有 provider）与 `<provider>`（只看该 provider）
/// 都读 `agent_dir()/sessions` 下全部已存会话——provider 是跨会话概念，只在当前会话里
/// 找某个 provider 基本等于无参统计。这只是取数范围，不决定报告内容范围。
fn collect_files(args: &Args) -> Result<Vec<(String, PathBuf)>, String> {
    if reads_every_saved_session(args) {
        let root = settings_manager::agent_dir().join("sessions");
        return Ok(all_session_files(&root));
    }

    let Some(path) = lock().session_file.clone() else {
        return Err(
            "No session file for this conversation yet. Use `/cache-usage all` to scan saved sessions."
                .to_string(),
        );
    };
    Ok(vec![(session_label(&path), path)])
}

/// 本次统计是否读取全部已存会话（见 [`collect_files`]；不代表报告内容范围）。
fn reads_every_saved_session(args: &Args) -> bool {
    args.all || args.provider.is_some()
}

/// 列出 `agent_dir()/sessions` 下所有工程的会话文件，按修改时间取最新一批。
fn all_session_files(root: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };

    let mut files: Vec<(SystemTime, String, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }

        for path in session_manager::list_sessions(&dir) {
            let modified = path
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            files.push((modified, session_label(&path), path));
        }
    }

    files.sort_by_key(|a| std::cmp::Reverse(a.0));
    files
        .into_iter()
        .take(MAX_SCAN_FILES)
        .map(|(_, label, path)| (label, path))
        .collect()
}

/// 会话的显示名：文件名（去扩展名）。
fn session_label(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string())
}

/// 打开面板并在后台完成扫描（无 tokio 运行时则就地扫描，便于单测）。
fn start_scan(args: Args, files: Vec<(String, PathBuf)>) {
    let id = next_ui_id();
    {
        let mut st = lock();
        st.overlay_id = Some(id);
        st.report = None;
        st.scanning = true;
        st.refresh_pending = false;
    }
    request_ui(ExtensionUiRequest::ShowOverlay { id });

    let scope = scope_label(&args, files.len());
    let filter = args.provider.clone();
    let scan = move || scan_files(&files, filter.as_deref(), scope);

    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            handle.spawn_blocking(move || finish_scan(id, scan()));
        }
        Err(_) => finish_scan(id, scan()),
    }
}

/// 扫描完成：面板还开着同一 id 时回填结果，并请求再重绘一帧。
///
/// 丢弃不匹配的结果（用户已关掉面板，或又发起了新一轮统计）。
fn finish_scan(id: u64, report: Report) {
    let mut st = lock();
    if st.overlay_id != Some(id) {
        return;
    }

    st.report = Some(report);
    st.scanning = false;
    st.refresh_pending = true;
}

/// 报告顶部显示的范围说明。
///
/// 三种范围同级：当前会话（空参）/ 全部会话（`all`）/ 指定 provider（`<provider>`，
/// 数据仍来自全部会话，但只留该 provider 的样本）。
fn scope_label(args: &Args, file_count: usize) -> String {
    match (&args.provider, args.all) {
        (Some(provider), _) => format!("provider {provider} · {file_count} files scanned"),
        (None, true) => format!("all sessions ({file_count} files)"),
        (None, false) => "current session".to_string(),
    }
}

/// 扫描一批会话文件并聚合成报告。
fn scan_files(files: &[(String, PathBuf)], provider_filter: Option<&str>, scope: String) -> Report {
    let mut seen: HashSet<String> = HashSet::new();
    let mut total = Group::default();
    let mut providers: HashMap<String, Group> = HashMap::new();
    let mut sessions: HashMap<String, Group> = HashMap::new();
    let mut samples: Vec<Sample> = Vec::new();

    for (label, path) in files {
        for sample in session_samples(path, &mut seen) {
            if let Some(filter) = provider_filter
                && sample.provider != filter
            {
                continue;
            }
            total.add(&sample);
            providers
                .entry(sample.provider.clone())
                .or_default()
                .add(&sample);
            sessions.entry(label.clone()).or_default().add(&sample);
            samples.push(sample);
        }
    }

    samples.sort_by_key(|s| s.timestamp);
    let trend: Vec<f64> = samples
        .iter()
        .map(|s| {
            let rate = Rate {
                input: s.input,
                read: s.read,
                write: s.write,
            };
            rate.percent()
        })
        .collect();

    Report {
        scope,
        total,
        providers: sort_groups(providers, usize::MAX),
        sessions: sort_groups(sessions, MAX_SESSION_ROWS),
        trend,
    }
}

/// 把分组按 token 总量降序排列，最多保留 `limit` 条。
fn sort_groups(groups: HashMap<String, Group>, limit: usize) -> Vec<(String, Group)> {
    let mut rows: Vec<(String, Group)> = groups.into_iter().collect();
    rows.sort_by(|a, b| {
        b.1.rate
            .total_input()
            .cmp(&a.1.rate.total_input())
            .then_with(|| a.0.cmp(&b.0))
    });
    rows.truncate(limit);
    rows
}

/// 从一个会话文件里取出用量样本（`seen` 用于跨会话去重）。
///
/// 文件打不开或需要修复时跳过——报告不是修复工具，不该因为一份坏文件整份失败。
fn session_samples(path: &Path, seen: &mut HashSet<String>) -> Vec<Sample> {
    let session = match Session::open_checked(&path.to_string_lossy()) {
        OpenSessionResult::Ok(session) => session,
        OpenSessionResult::Invalid(_) | OpenSessionResult::Repairable { .. } => return Vec::new(),
    };

    let mut out = Vec::new();
    let mut last_provider: Option<String> = None;

    for entry in session.state.entries() {
        match entry {
            Entry::ModelChange { provider, .. } => last_provider = Some(provider.clone()),
            Entry::Message { base, message, .. } => {
                if message.role != "assistant" {
                    continue;
                }
                let provider = message
                    .provider
                    .clone()
                    .or_else(|| last_provider.clone())
                    .unwrap_or_else(|| "(unknown)".to_string());
                // 记住最近一次带 provider 的消息：没有 ModelChange 的旧会话也能给
                // 后面的压缩条目归到正确的 provider。
                if message.provider.is_some() {
                    last_provider = Some(provider.clone());
                }
                if let Some(sample) = sample_from(
                    &provider,
                    base.id.as_str(),
                    base.timestamp,
                    message.usage.as_ref(),
                    seen,
                ) {
                    out.push(sample);
                }
            }
            Entry::Compaction { base, usage, .. } | Entry::BranchSummary { base, usage, .. } => {
                let provider = last_provider
                    .clone()
                    .unwrap_or_else(|| "(unknown)".to_string());
                if let Some(sample) = sample_from(
                    &provider,
                    base.id.as_str(),
                    base.timestamp,
                    usage.as_ref(),
                    seen,
                ) {
                    out.push(sample);
                }
            }
            _ => {}
        }
    }

    out
}

/// 由一个条目的 usage 构造样本；没有用量、全零或重复（`seen` 命中）时返回 `None`。
fn sample_from(
    provider: &str,
    entry_id: &str,
    timestamp: i64,
    usage: Option<&Usage>,
    seen: &mut HashSet<String>,
) -> Option<Sample> {
    let usage = usage?;
    let input = usage.input as u64;
    let read = usage.cache_read as u64;
    let write = usage.cache_write as u64;
    if input == 0 && read == 0 && write == 0 {
        return None;
    }

    // 去重键照 kiss（`cache.rs`）的口径：同一条目在 fork 出来的会话文件里会重复出现
    let key = format!("{entry_id}|{timestamp}|{provider}|{input}|{read}|{write}");
    if !seen.insert(key) {
        return None;
    }

    Some(Sample {
        provider: provider.to_string(),
        input,
        read,
        write,
        cost: usage.cost.total,
        timestamp,
    })
}

/// 报告面板的全部行（渲染与滚动上限共用同一份）。
fn report_lines(report: &Report) -> Vec<DockLine> {
    let mut lines: Vec<DockLine> = Vec::new();

    // 面板样式不渲染 `title`（只有 Inline 才有标题行），标题信息落在内容首行
    lines.push(vec![DockSpan::new(
        ACCENT.0,
        ACCENT.1,
        format!(
            "Cache Usage · {} · {} responses · ${:.4}",
            report.scope, report.total.responses, report.total.cost
        ),
    )]);
    lines.push(Vec::new());
    lines.push(total_line(&report.total));
    if report.total.responses > 0 {
        lines.push(Vec::new());
        lines.push(vec![DockSpan::new(
            MUTED.0,
            MUTED.1,
            "hit rate over time (left = oldest)",
        )]);
        lines.push(vec![DockSpan::new(
            ACCENT.0,
            ACCENT.1,
            sparkline(&report.trend, TREND_COLS),
        )]);
    }

    if report.providers.len() > 1 {
        lines.push(Vec::new());
        lines.push(vec![DockSpan::new(ACCENT.0, ACCENT.1, "by provider")]);
        for (name, group) in &report.providers {
            lines.push(group_line(name, group, 22));
        }
    }

    if !report.sessions.is_empty() {
        lines.push(Vec::new());
        lines.push(vec![DockSpan::new(ACCENT.0, ACCENT.1, "by session")]);
        for (name, group) in &report.sessions {
            lines.push(group_line(name, group, 22));
        }
    }

    if report.total.responses == 0 {
        lines.push(Vec::new());
        lines.push(vec![DockSpan::new(
            DIM.0,
            DIM.1,
            "no measured cache usage in this scope",
        )]);
    }

    lines
}

/// 总量行：条形图 + 命中率 + token 明细。
fn total_line(group: &Group) -> DockLine {
    let percent = group.rate.percent();
    vec![
        DockSpan::new(ACCENT.0, ACCENT.1, rate_bar(percent, BAR_WIDTH)),
        DockSpan::plain(format!(
            " {percent:5.1}%  {} / {} input{}",
            crate::extensions::util::format_tokens(group.rate.read),
            crate::extensions::util::format_tokens(group.rate.total_input()),
            if group.responses > 0 {
                format!(" · {} responses", group.responses)
            } else {
                String::new()
            }
        )),
    ]
}

/// 分组行：名称 + 条形图 + 命中率 + token 明细。
fn group_line(name: &str, group: &Group, name_width: usize) -> DockLine {
    let label = crate::extensions::util::truncate_chars(name, name_width);
    let percent = group.rate.percent();
    let mut spans = vec![DockSpan::plain(format!("{label:<name_width$} "))];
    spans.push(DockSpan::new(
        ACCENT.0,
        ACCENT.1,
        rate_bar(percent, BAR_WIDTH),
    ));
    spans.push(DockSpan::new(
        MUTED.0,
        MUTED.1,
        format!(
            " {percent:5.1}%  {} / {}",
            crate::extensions::util::format_tokens(group.rate.read),
            crate::extensions::util::format_tokens(group.rate.total_input())
        ),
    ));
    spans
}

/// 命中率条形图：`width` 格里按百分比填充实心格（四舍五入）。
fn rate_bar(percent: f64, width: usize) -> String {
    let ratio = (percent / 100.0).clamp(0.0, 1.0);
    let filled = (ratio * width as f64).round() as usize;
    let filled = filled.min(width);
    let mut out = String::with_capacity(width);
    for i in 0..width {
        out.push(if i < filled {
            DEF_BAR_FULL
        } else {
            DEF_BAR_EMPTY
        });
    }
    out
}

/// 命中率趋势 sparkline：把样本按时间均分到 `cols` 列，每列取平均后映射到 8 档块字符。
///
/// 样本数少于列数时左侧留空格（时间轴右对齐到最新）。
fn sparkline(percents: &[f64], cols: usize) -> String {
    if percents.is_empty() || cols == 0 {
        return String::new();
    }

    let mut out = String::with_capacity(cols);
    if percents.len() <= cols {
        for _ in 0..(cols - percents.len()) {
            out.push(' ');
        }
        for percent in percents {
            out.push(level_char(*percent));
        }
        return out;
    }

    for col in 0..cols {
        let start = col * percents.len() / cols;
        let end = ((col + 1) * percents.len() / cols).max(start + 1);
        let slice = &percents[start..end.min(percents.len())];
        #[allow(clippy::cast_precision_loss)]
        let avg = slice.iter().sum::<f64>() / slice.len() as f64;
        out.push(level_char(avg));
    }
    out
}

/// 百分比 → 8 档块字符。
fn level_char(percent: f64) -> char {
    let ratio = (percent / 100.0).clamp(0.0, 1.0);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let level = (ratio * (DEF_SPARK_BLOCKS.len() - 1) as f64).round() as usize;
    DEF_SPARK_BLOCKS[level.min(DEF_SPARK_BLOCKS.len() - 1)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        core::{
            extensions::OverlayKey,
            provider::{AgentMessage, Cost},
        },
        test_support::{AUTH_TEST_LOCK, AgentDirGuard},
    };

    /// 全局状态锁（多个用例共享进程级 state）。
    fn lock_global() -> MutexGuard<'static, ()> {
        AUTH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 一条带用量的 assistant 消息。
    fn assistant(input: u32, read: u32, write: u32, cost: f64) -> AgentMessage {
        let mut message = AgentMessage::user_text("ok");
        message.role = "assistant".to_string();
        message.stop_reason = Some("stop".to_string());
        message.model = Some("claude-fable-5".to_string());
        message.provider = Some("anthropic".to_string());
        message.usage = Some(usage(input, read, write, cost));
        message
    }

    /// 一份用量记录。
    fn usage(input: u32, read: u32, write: u32, cost: f64) -> Usage {
        Usage {
            input,
            output: 10,
            cache_read: read,
            cache_write: write,
            cache_write_1h: None,
            reasoning: None,
            total_tokens: input + read + write + 10,
            cost: Cost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
                total: cost,
            },
        }
    }

    /// 写一份会话文件，返回路径（含两条 assistant 用量 + 一条压缩用量）。
    fn write_session(dir: &Path, with_compaction: bool) -> PathBuf {
        let mut session = Session::create("", Some(dir.to_path_buf()), true).unwrap();
        session.append_message(&assistant(1000, 1000, 0, 0.01));
        session.append_message(&assistant(0, 3000, 0, 0.02));
        if with_compaction {
            let usage = usage(100, 0, 0, 0.03);
            session.append_compaction("summary", 10, None, None, Some(&usage));
        }
        session.get_session_file().unwrap().to_path_buf()
    }

    #[test]
    fn rate_bar_fills_by_percentage() {
        assert_eq!(rate_bar(0.0, 4), "░░░░");
        assert_eq!(rate_bar(100.0, 4), "████");
        assert_eq!(rate_bar(50.0, 4), "██░░");
        assert_eq!(
            rate_bar(1.0, 24)
                .chars()
                .filter(|c| *c == DEF_BAR_FULL)
                .count(),
            0
        );
        assert_eq!(
            rate_bar(99.0, 24)
                .chars()
                .filter(|c| *c == DEF_BAR_FULL)
                .count(),
            24
        );
        assert_eq!(rate_bar(150.0, 4), "████", "超过 100% 也封顶");
    }

    #[test]
    fn rate_percent_uses_read_over_total_input() {
        let rate = Rate {
            input: 100,
            read: 300,
            write: 100,
        };
        assert_eq!(rate.total_input(), 500);
        assert!((rate.percent() - 60.0).abs() < 1e-9);
        assert_eq!(Rate::default().percent(), 0.0, "无样本时不该除零");
    }

    #[test]
    fn sparkline_maps_levels_and_pads_oldest() {
        // 两个样本 < 4 列：左侧补空格，时间轴右对齐
        assert_eq!(sparkline(&[0.0, 100.0], 4), "  ▁█");
        // 均值分桶：一列一个样本（8 档四舍五入：0% → ▁、50% → ▅、100% → █）
        assert_eq!(sparkline(&[0.0, 50.0, 100.0], 3), "▁▅█");
        assert_eq!(sparkline(&[], 4), "");
    }

    #[test]
    fn parse_args_accepts_all_and_provider_in_any_order() {
        assert_eq!(parse_args(""), Ok(Args::default()));
        assert_eq!(
            parse_args("all"),
            Ok(Args {
                all: true,
                provider: None
            })
        );
        assert_eq!(
            parse_args("anthropic all"),
            Ok(Args {
                all: true,
                provider: Some("anthropic".to_string())
            })
        );
        assert!(
            parse_args("all openai anthropic").is_err(),
            "两个 provider 应报错"
        );
    }

    #[test]
    fn scan_aggregates_usage_and_dedupes_forks() {
        let _g = lock_global();
        let _ad = AgentDirGuard::temp();
        let dir = std::env::temp_dir().join(format!("prux-cache-usage-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = write_session(&dir, true);
        let label = session_label(&path);

        let files = vec![(label.clone(), path.clone())];
        let report = scan_files(&files, None, "test".to_string());

        // 1000/(1000+1000) + 3000/(0+3000) + 100/(100+0) 三次响应
        assert_eq!(report.total.responses, 3);
        assert_eq!(report.total.rate.input, 1100);
        assert_eq!(report.total.rate.read, 4000);
        assert!((report.total.cost - 0.06).abs() < 1e-9);
        assert_eq!(report.providers.len(), 1);
        assert_eq!(report.providers[0].0, "anthropic");
        assert_eq!(report.sessions.len(), 1);
        assert_eq!(report.sessions[0].0, label);
        assert_eq!(report.trend.len(), 3);

        // 同一份文件按两个会话名扫两遍：去重后总量不变（模拟 fork 里的重复条目）
        let dupes = vec![
            (label.clone(), path.clone()),
            (format!("{label}-fork"), path.clone()),
        ];
        let deduped = scan_files(&dupes, None, "test".to_string());
        assert_eq!(deduped.total.responses, 3, "重复条目应被去重");
        assert_eq!(deduped.sessions.len(), 1, "重复条目不该造出第二个会话分组");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn scan_filters_by_provider() {
        let _g = lock_global();
        let _ad = AgentDirGuard::temp();
        let dir =
            std::env::temp_dir().join(format!("prux-cache-usage-filter-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = write_session(&dir, false);

        let files = vec![(session_label(&path), path.clone())];
        let filtered = scan_files(&files, Some("openai"), "test".to_string());
        assert_eq!(filtered.total.responses, 0);
        assert!(filtered.providers.is_empty());

        let hit = scan_files(&files, Some("anthropic"), "test".to_string());
        assert_eq!(hit.total.responses, 2);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn provider_or_all_scans_every_saved_session() {
        let _g = lock_global();
        let _ad = AgentDirGuard::temp();
        let project = settings_manager::agent_dir()
            .join("sessions")
            .join("--proj--");
        std::fs::create_dir_all(&project).unwrap();
        let saved = write_session(&project, false);

        // 当前会话就是同一份文件：两种写法都能收集到它
        remember_session(Some(saved.clone()));
        let only_current = collect_files(&Args::default()).unwrap();
        assert_eq!(only_current.len(), 1);

        // 给了 provider 就等于跨全部会话：即使当前会话还没落盘也能扫
        remember_session(None);
        let by_provider = collect_files(&Args {
            all: false,
            provider: Some("anthropic".to_string()),
        })
        .unwrap();
        assert_eq!(by_provider.len(), 1, "provider 查询应扫已存会话");
        assert!(
            collect_files(&Args::default()).is_err(),
            "空参仍要求当前会话"
        );

        let all = collect_files(&Args {
            all: true,
            provider: None,
        })
        .unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(scope_label(&Args::default(), 1), "current session");
        assert_eq!(
            scope_label(
                &Args {
                    all: true,
                    provider: None
                },
                7
            ),
            "all sessions (7 files)"
        );
        assert_eq!(
            scope_label(
                &Args {
                    all: false,
                    provider: Some("anthropic".to_string())
                },
                7
            ),
            "provider anthropic · 7 files scanned"
        );
    }

    #[test]
    fn report_lines_include_total_providers_and_sessions() {
        let _g = lock_global();
        let _ad = AgentDirGuard::temp();
        let dir =
            std::env::temp_dir().join(format!("prux-cache-usage-lines-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = write_session(&dir, false);

        let report = scan_files(
            &[(session_label(&path), path.clone())],
            None,
            "current session".to_string(),
        );
        let text: Vec<String> = report_lines(&report)
            .iter()
            .map(|line| {
                line.iter()
                    .map(|span| span.text.clone())
                    .collect::<String>()
            })
            .collect();
        let joined = text.join("\n");
        assert!(joined.contains("current session"));
        assert!(joined.contains("hit rate over time"));
        assert!(joined.contains("by session"), "应列出各会话");
        assert!(!joined.contains("by provider"), "单 provider 时不重复列");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn overlay_shows_scaffold_then_report() {
        let _g = lock_global();
        let _ad = AgentDirGuard::temp();
        drain_ui();

        // 空参且没有会话文件：命令应提示而不是打开面板
        let mut st = App::new();
        remember_session(None);
        assert!(!command_cache_usage(&mut st, "cache-usage"));
        assert_eq!(lock().overlay_id, None);

        // 注入一份报告，覆盖层应认领并渲染
        let report = Report {
            scope: "current session".to_string(),
            total: Group {
                rate: Rate {
                    input: 10,
                    read: 90,
                    write: 0,
                },
                cost: 0.5,
                responses: 1,
            },
            providers: Vec::new(),
            sessions: vec![(
                "sess-1".to_string(),
                Group {
                    rate: Rate {
                        input: 10,
                        read: 90,
                        write: 0,
                    },
                    cost: 0.5,
                    responses: 1,
                },
            )],
            trend: vec![90.0],
        };
        let id = next_ui_id();
        {
            let mut state = lock();
            state.overlay_id = Some(id);
            state.report = Some(report);
            state.scanning = false;
        }

        let ext = CacheUsage;
        assert_eq!(ext.hooks(), vec![ExtensionHook::Overlay]);
        let view = ext.overlay_view(id, 80).expect("认领自己的 id");
        assert!(view.title.starts_with("Cache usage"));
        assert!(matches!(view.size, OverlaySize::Panel { .. }));
        assert!(ext.overlay_view(id + 1, 80).is_none());

        // 滚动交给 TUI：扩展不消费 ↑↓/PageDown（自己 skip 会让面板高度跟着缩）
        assert!(!ext.on_overlay_event(
            id,
            &OverlayEvent::Key {
                key: OverlayKey::Down,
                input: None,
            }
        ));

        // 行是全量的，视窗由 TUI 按 max_rows 裁；趋势标题前留空行
        let all = report_lines(lock().report.as_ref().unwrap());
        let joined: Vec<String> = all
            .iter()
            .map(|line| line.iter().map(|s| s.text.as_str()).collect())
            .collect();
        assert_eq!(view.lines.len(), all.len());
        let trend_at = joined
            .iter()
            .position(|l| l.contains("hit rate over time"))
            .expect("有趋势标题");
        assert!(joined[trend_at - 1].is_empty(), "趋势标题前应有一行间隔");
        assert!(joined[0].starts_with("Cache Usage · "), "首行是标题行");

        assert!(ext.on_overlay_event(id, &OverlayEvent::Closed));
        assert_eq!(lock().overlay_id, None);
    }

    #[test]
    fn finished_scan_still_wants_one_frame() {
        let _g = lock_global();
        let id = next_ui_id();
        {
            let mut state = lock();
            state.overlay_id = Some(id);
            state.report = None;
            state.scanning = true;
            state.refresh_pending = false;
        }
        assert!(CacheUsage.wants_redraw(), "扫描中要重绘");

        // 后台扫描完成：scanning 归 false，但结果还没渲染过，必须仍要一帧，
        // 否则空闲时 TUI 不会再刷（80ms tick 只轮询 wants_redraw）。
        finish_scan(id, sample_report());
        assert!(!lock().scanning);
        assert!(CacheUsage.wants_redraw(), "结果就绪后界面还得刷一次");

        assert!(CacheUsage.overlay_view(id, 80).is_some());
        assert!(!CacheUsage.wants_redraw(), "渲染过一帧后不再持续重绘");

        // 面板已关掉时扫描才回来：结果丢弃，不能留下永久重绘
        assert!(CacheUsage.on_overlay_event(id, &OverlayEvent::Closed));
        finish_scan(id, sample_report());
        assert!(lock().report.is_none());
        assert!(!CacheUsage.wants_redraw());
    }

    /// 最小可渲染报告（只要求行数与字段都存在）。
    fn sample_report() -> Report {
        Report {
            scope: "current session".to_string(),
            total: Group {
                rate: Rate {
                    input: 10,
                    read: 90,
                    write: 0,
                },
                cost: 0.5,
                responses: 1,
            },
            providers: Vec::new(),
            sessions: vec![(
                "sess-1".to_string(),
                Group {
                    rate: Rate {
                        input: 10,
                        read: 90,
                        write: 0,
                    },
                    cost: 0.5,
                    responses: 1,
                },
            )],
            trend: vec![90.0],
        }
    }

    #[test]
    fn disabled_extension_clears_state() {
        let _g = lock_global();
        let id = next_ui_id();
        {
            let mut state = lock();
            state.overlay_id = Some(id);
            state.scanning = true;
        }
        CacheUsage.on_enabled_changed(false);
        assert_eq!(lock().overlay_id, None);
        assert!(!CacheUsage.wants_redraw());
        assert!(CacheUsage.hooks().is_empty());
    }

    #[test]
    fn command_declares_usage_surface() {
        let cmds = CacheUsage.commands();
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].name, "cache-usage");
        assert!(cmds[0].busy_safe);
        assert_eq!(cmds[0].subcommands[0].name, "all");
        assert_eq!(CacheUsage.name(), EXT);
    }

    /// 清空待处理的 UI 请求队列（避免用例间互相 `take_pending_ui`）。
    fn drain_ui() {
        while crate::core::extensions::take_pending_ui().is_some() {}
    }
}
