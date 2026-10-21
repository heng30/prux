//! `vet` 扩展：收工前的项目检查闭环（green loop）。
//!
//! **问题**：agent 说「改好了」并不等于它真的过了测试。用户每轮都要自己跑一遍
//! `cargo test` / `npm test`，红了再把输出贴回去让它修，来回几次才收敛。
//!
//! **本扩展**：在 agent 结算前（[`ExtensionHook::Boundary`] 的 `agent_before_settle`）
//! 自动跑用户配置的检查命令；有阻塞级失败就把失败输出作为一条 user 消息注入并**续跑一轮**，
//! 直到全绿、达到迭代上限、或发现同一条失败反复出现。
//!
//! 三条收敛条件（缺一就会无限烧 token）：
//!
//! | 条件 | 行为 |
//! |---|---|
//! | 跑满 `maxIterations` 次 | 停止续跑，提示「仍失败」，交回用户 |
//! | 失败指纹与上次相同 | 停止续跑（同一条错误再喂也没用） |
//! | 用户中断（`outcome == aborted`）/ 有排队输入 | 不续跑，让位给用户 |
//!
//! 其它边界：
//! - 只在主 agent 结算时动作（子代理的 `settle` 不跑检查）；
//! - `requireEdits` 开时，本轮没调用过写类工具（`edit` / `write` / `bash` / `powershell`）
//!   就直接跳过——纯问答轮不该跑测试；
//! - `optional` 的检查失败只报告、不阻止收工；
//! - 同一时刻只允许一组检查在跑（自动路径与 `/vet run` 共享一把闸）。
//!
//! 配置在 `agent_dir()/extensions/vet.json` 与项目 `.prux/extensions/vet.json`（仅受信项目），
//! 见 [`config`]；执行与报告渲染见 [`run`]；`/vet` 命令族见 [`command`]。
//!
//! **默认不启用**，声明 `Dev` 模式（`Minimal` 下不可用）——跑测试是分钟级操作，
//! 只能由用户显式打开。

mod command;
mod config;
mod run;

use crate::{
    core::{
        extensions::{
            BeforeToolCallOutcome, BoundaryOutcome, Extension, ExtensionCommand, ExtensionHook,
            ExtensionMode, ExtensionSetting, ExtensionTool, UiNotifyLevel,
        },
        provider::AgentMessage,
        session_manager::Session,
        tools::ToolError,
    },
    extensions::{
        EXTENSION_FACTORIES, ExtensionFactory, PRIORITY_VET,
        util::{notify_text, truncate_chars},
    },
    modes::interactive::handlers::register_slash_command,
    utils::{glyphs::DEF_TRAILING_ELLIPSIS, paths::cwd, time::format_duration_ms},
};
use config::{Announce, Config, EXT};
use run::CheckResult;
use serde_json::Value;
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

/// 视为「可能改动工作区」的工具：调用过它们才认为本轮值得跑检查。
///
/// `bash` / `powershell` 在列是保守估计——一条 `ls` 也会置位，代价只是多跑一次检查；
/// 反过来（漏判）会让 green loop 在该跑的时候不跑。
const WRITE_TOOLS: &[&str] = &["edit", "write", "bash", "powershell"];

/// 自声明工厂：linkme 分布式切片。
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static VET_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_VET,
    make: || -> Arc<dyn Extension> { Arc::new(Vet) },
};

/// 当前会话的 cwd（`on_session_start` 记录；拿不到时回落到进程 cwd）。
static SESSION_CWD: Mutex<Option<PathBuf>> = Mutex::new(None);

/// 本扩展的回合内运行态。
static STATE: Mutex<State> = Mutex::new(State::new());

/// 检查执行闸：同一时刻只允许一组检查在跑（自动路径与 `/vet run` 共享）。
static RUNNING: AtomicBool = AtomicBool::new(false);

/// 配置告警只提示一次的开关（避免每次结算都刷屏）。
static WARNED: AtomicBool = AtomicBool::new(false);

/// `/vet add` 的选项：写入项目配置而非全局。
const ADD_PROJECT: &str = "--project";
/// `/vet add` 的选项：把这条检查标为可选（失败不阻止收工）。
const ADD_OPTIONAL: &str = "--optional";
/// `/vet detect` 的选项：直接把候选写进全局配置。
const DETECT_WRITE: &str = "--write";

/// 回合内运行态（全部在 [`STATE`] 里）。
struct State {
    /// 本轮已自动续跑的次数（用户再次提交 / 检查全绿 / 放弃时复位）。
    iteration: usize,
    /// 本轮是否调用过写类工具。
    dirty: bool,
    /// 上次注入模型的失败指纹（用于识别「同一条失败反复出现」）。
    last_failure: Option<u64>,
    /// 最近一次检查的结论一句话（`/vet status` 展示）。
    last_verdict: Option<String>,
    /// 最近一次检查结束的时刻（展示「多久之前」）。
    last_run_at: Option<Instant>,
    /// 累计跑过多少次检查（`/vet status` 展示）。
    runs: usize,
}

impl State {
    /// 全零初值。
    const fn new() -> Self {
        State {
            iteration: 0,
            dirty: false,
            last_failure: None,
            last_verdict: None,
            last_run_at: None,
            runs: 0,
        }
    }
}

/// 取运行态；锁被毒化时取回内部数据继续用。
fn state() -> MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// 清空回合内状态（保留「最近一次结论」这类跨回合信息）。
fn reset_turn_state() {
    let mut st = state();
    st.iteration = 0;
    st.dirty = false;
    st.last_failure = None;
}

/// 记下本次会话的 cwd。
fn remember_cwd(cwd: &str) {
    *SESSION_CWD.lock().unwrap_or_else(|e| e.into_inner()) = Some(PathBuf::from(cwd));
}

/// 当前生效的 cwd：会话记录的优先，其次进程 cwd，都拿不到时用 `.`。
fn current_cwd() -> PathBuf {
    if let Some(cwd) = SESSION_CWD
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
    {
        return cwd;
    }
    cwd()
}

/// 是否写类工具（见 [`WRITE_TOOLS`]）。
fn is_write_tool(name: &str) -> bool {
    WRITE_TOOLS.contains(&name)
}

/// 检查执行闸的持有凭证：drop 时自动放行。
struct RunGuard;

impl RunGuard {
    /// 抢占执行闸；已有检查在跑时返回 `None`。
    fn acquire() -> Option<RunGuard> {
        RUNNING
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .ok()
            .map(|_| RunGuard)
    }
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        RUNNING.store(false, Ordering::SeqCst);
    }
}

/// 把检查项名字列成一行（最多 4 条，多余的折叠成 `+N`）。
fn check_names(cfg: &Config) -> String {
    let names: Vec<String> = cfg
        .checks
        .iter()
        .take(4)
        .map(|c| truncate_chars(&c.name, 24))
        .collect();
    let extra = cfg.checks.len().saturating_sub(names.len());
    let mut text = names.join(", ");
    if extra > 0 {
        text.push_str(&format!(" +{extra} more"));
    }
    text
}

/// 配置解析告警：每个进程只提示一次。
fn surface_warnings(cfg: &Config) {
    if cfg.warnings.is_empty() || WARNED.swap(true, Ordering::Relaxed) {
        return;
    }
    for warning in &cfg.warnings {
        notify_text(&format!("vet: {warning}"), UiNotifyLevel::Warning);
    }
}

/// `vet` 扩展。
pub struct Vet;

impl Extension for Vet {
    /// 扩展注册名（`/extension` 面板与 `/vet` 命令共用）。
    fn name(&self) -> &str {
        EXT
    }

    /// 面板里的功能说明。
    fn description(&self) -> &str {
        "Green loop: run the project's checks (test / lint / typecheck) when the agent finishes \
         and feed failures back so it keeps fixing until they pass. Checks come from \
         agent_dir()/extensions/vet.json plus a trusted project's .prux/extensions/vet.json; \
         /vet run|status|detect|add|remove|reset"
    }

    /// 仅在 Dev 及以上模式可用。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Dev]
    }

    /// 默认不启用：跑测试是分钟级操作，属于用户显式选择的能力。
    fn default_enabled(&self) -> bool {
        false
    }

    /// 不提供工具：本扩展只在结算边界动作，模型看不到它。
    fn tools(&self) -> Vec<ExtensionTool> {
        Vec::new()
    }

    /// 声明 `/vet` 命令（run / status / detect / add / remove / reset）。
    fn commands(&self) -> Vec<ExtensionCommand> {
        vec![ExtensionCommand {
            name: config::CMD.to_string(),
            description: "Green loop checks: /vet run|status|detect|add|remove|reset".to_string(),
            // handler 只读写本扩展自身的配置与运行态，不锁 agent。
            busy_safe: true,
            subcommands: command::SUBCOMMANDS.to_vec(),
        }]
    }

    /// 接线 `/vet` 执行入口。
    fn on_registered(&self) {
        register_slash_command(EXT, config::CMD, command::handle);
    }

    /// 订阅结算边界（自动跑检查）、工具前钩子（记脏标记）与提交通知（复位回合状态）。
    fn hooks(&self) -> Vec<ExtensionHook> {
        vec![
            ExtensionHook::Boundary,
            ExtensionHook::BeforeToolCall,
            ExtensionHook::UserPrompt,
        ]
    }

    /// 启用时先加载一次配置（把告警提示出去）；禁用时清空回合状态。
    fn on_enabled_changed(&self, enabled: bool) {
        if enabled {
            let cfg = config::load(&current_cwd());
            surface_warnings(&cfg);
        } else {
            reset_turn_state();
            WARNED.store(false, Ordering::Relaxed);
        }
    }

    /// 记录会话 cwd，并复位回合状态（新会话不继承上一次的续跑计数）。
    fn on_session_start(&self, cwd: &str, _messages: &[AgentMessage], _session: Option<&Session>) {
        remember_cwd(cwd);
        reset_turn_state();
        WARNED.store(false, Ordering::Relaxed);
    }

    /// 会话切换后项目配置可能换了，重新记 cwd 并让告警可以再提示一次。
    fn on_session_switched(&self, _session_path: Option<&str>, _messages: &[AgentMessage]) {
        reset_turn_state();
        WARNED.store(false, Ordering::Relaxed);
    }

    /// 用户（重新）提交输入：这是一个新的回合意图，续跑计数与失败指纹一起复位。
    fn on_user_submit(&self, _text: &str, _busy: bool) {
        reset_turn_state();
    }

    /// 工具调用前只做一件事：把「本轮改过东西」记下来（不拦截、不改写参数）。
    fn before_tool_call_ext(
        &self,
        name: &str,
        _args: &Value,
        _assistant_message: Option<&AgentMessage>,
        _context: &[AgentMessage],
        _all_args: &Value,
    ) -> std::result::Result<BeforeToolCallOutcome, ToolError> {
        if is_write_tool(name) {
            state().dirty = true;
        }
        Ok(BeforeToolCallOutcome::allow())
    }

    /// 结算边界：跑检查、失败则注入报告并续跑（收敛条件见模块文档）。
    fn on_boundary(&self, event: &Value) -> Option<BoundaryOutcome> {
        if event.get("type").and_then(Value::as_str) != Some("agent_before_settle") {
            return None;
        }
        // 子代理的结算不跑检查：绿不绿是主 agent 的事
        if event.get("agentScope").and_then(Value::as_str) != Some("main") {
            return None;
        }
        // 用户中断：不续跑，也不把计数留着污染下一回合
        if event.get("outcome").and_then(Value::as_str) == Some("aborted") {
            reset_turn_state();
            return None;
        }
        // 有排队输入：让位给用户，由 UI 起新一轮
        if event
            .get("hasQueuedMessages")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return None;
        }

        settle()
    }

    /// 该扩展的可调开关（`/extension` 面板里的设置项）。
    fn settings(&self) -> Vec<ExtensionSetting> {
        config::panel_settings(&config::load(&current_cwd()))
    }

    /// 应用设置面板的变更（写入全局配置并立即生效）。
    fn apply_setting(&self, key: &str, value: &str) -> std::result::Result<(), String> {
        config::apply_panel_choice(&current_cwd(), key, value)
            .map_err(|e| format!("write config failed: {e}"))
    }
}

/// 结算边界的完整流程。返回 `Some(continue_with)` 表示注入失败报告并再跑一轮。
fn settle() -> Option<BoundaryOutcome> {
    let cwd = current_cwd();
    let cfg = config::load(&cwd);
    surface_warnings(&cfg);

    if !cfg.auto_run || !cfg.has_checks() {
        return None;
    }

    // 取出并消费脏标记：同一批改动只跑一次检查
    let (iteration, dirty) = {
        let mut st = state();
        let dirty = st.dirty;
        st.dirty = false;
        (st.iteration, dirty)
    };
    if cfg.require_edits && !dirty {
        return None;
    }

    let guard = RunGuard::acquire()?;

    if cfg.announce != Announce::Off {
        notify_text(
            &format!(
                "vet: running {} check{} ({}) {DEF_TRAILING_ELLIPSIS}",
                cfg.checks.len(),
                if cfg.checks.len() == 1 { "" } else { "s" },
                check_names(&cfg)
            ),
            UiNotifyLevel::Info,
        );
    }

    let results = run::run_all(&cwd, &cfg.checks, cfg.timeout);
    drop(guard);
    record_run(&results);

    if !run::has_blocking_failure(&results) {
        if cfg.announce == Announce::Always {
            notify_text(&run::verdict(&results), UiNotifyLevel::Success);
        }
        reset_turn_state();
        return None;
    }

    let summary = run::verdict(&results);
    if cfg.announce != Announce::Off {
        notify_text(&summary, UiNotifyLevel::Warning);
    }

    // 收敛 1：跑满上限 → 交回用户
    if iteration >= cfg.max_iterations {
        reset_turn_state();
        notify_text(
            &format!(
                "vet: stopping after {} attempt{} — {summary}. Run /vet run to retry, or /vet \
                 status to review the configuration.",
                iteration,
                if iteration == 1 { "" } else { "s" }
            ),
            UiNotifyLevel::Warning,
        );
        return None;
    }

    // 收敛 2：同一条失败反复出现 → 再喂一次也没用
    let fingerprint = run::failure_fingerprint(&results);
    if fingerprint.is_some() && fingerprint == state().last_failure {
        reset_turn_state();
        notify_text(
            &format!("vet: the same failures repeated — stopping the loop. {summary}"),
            UiNotifyLevel::Warning,
        );
        return None;
    }

    let attempt = iteration + 1;
    {
        let mut st = state();
        st.iteration = attempt;
        st.last_failure = fingerprint;
    }

    let report = run::failure_report(&results, attempt, cfg.max_iterations);
    Some(BoundaryOutcome::continue_with(vec![
        AgentMessage::user_text(&report),
    ]))
}

/// 记录一次检查的结论（`/vet status` 用）。
fn record_run(results: &[CheckResult]) {
    let mut st = state();
    st.last_verdict = Some(run::verdict(results));
    st.last_run_at = Some(Instant::now());
    st.runs += 1;
}

/// `/vet status` 展示「上次检查」的相对时间。
fn last_run_label(at: Option<Instant>) -> String {
    match at {
        Some(at) => format!(
            "{} ago",
            format_duration_ms(at.elapsed().as_millis() as u64)
        ),
        None => "never".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 复位全局状态并造一份干净配置环境。
    fn fresh() -> crate::test_support::HomeGuard {
        let guard = crate::test_support::HomeGuard::temp();
        reset_turn_state();
        {
            let mut st = state();
            st.last_verdict = None;
            st.last_run_at = None;
            st.runs = 0;
        }
        WARNED.store(false, Ordering::Relaxed);
        guard
    }

    #[test]
    fn extension_declares_the_green_loop_surface() {
        let _g = fresh();
        let ext = Vet;
        assert_eq!(ext.name(), "vet");
        assert!(!ext.default_enabled(), "跑测试必须由用户显式打开");
        assert_eq!(ext.modes(), vec![ExtensionMode::Dev]);
        assert!(ext.tools().is_empty(), "本扩展不给模型加工具");
        let hooks = ext.hooks();
        assert!(hooks.contains(&ExtensionHook::Boundary));
        assert!(hooks.contains(&ExtensionHook::BeforeToolCall));
        assert!(hooks.contains(&ExtensionHook::UserPrompt));
        let commands = ext.commands();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].name, "vet");
        assert!(!commands[0].subcommands.is_empty());
    }

    #[test]
    fn subcommands_match_the_parser() {
        // 候选列表与 handler 的 match 分支必须一致，否则输入框提示会骗人
        let names: Vec<&str> = command::SUBCOMMANDS.iter().map(|s| s.name).collect();
        assert_eq!(
            names,
            vec!["run", "status", "detect", "add", "remove", "reset"]
        );
    }

    #[test]
    fn before_tool_call_only_flags_write_tools() {
        let _g = fresh();
        let ext = Vet;
        ext.before_tool_call_ext("read", &Value::Null, None, &[], &Value::Null)
            .unwrap();
        assert!(!state().dirty, "读类工具不该置位");

        ext.before_tool_call_ext("edit", &Value::Null, None, &[], &Value::Null)
            .unwrap();
        assert!(state().dirty, "写类工具应置位");
    }

    #[test]
    fn boundary_ignores_non_settle_events_and_child_agents() {
        let _g = fresh();
        let ext = Vet;
        assert!(
            ext.on_boundary(&serde_json::json!({"type": "turn_end"}))
                .is_none()
        );
        assert!(
            ext.on_boundary(&serde_json::json!({
                "type": "agent_before_settle",
                "agentScope": "child"
            }))
            .is_none()
        );
    }

    #[test]
    fn boundary_skips_when_queued_input_or_aborted() {
        let _g = fresh();
        state().dirty = true;
        let ext = Vet;
        assert!(
            ext.on_boundary(&serde_json::json!({
                "type": "agent_before_settle",
                "agentScope": "main",
                "outcome": "aborted"
            }))
            .is_none()
        );
        assert!(
            ext.on_boundary(&serde_json::json!({
                "type": "agent_before_settle",
                "agentScope": "main",
                "hasQueuedMessages": true
            }))
            .is_none()
        );
    }

    #[test]
    fn user_submit_resets_the_turn_state() {
        let _g = fresh();
        {
            let mut st = state();
            st.iteration = 2;
            st.dirty = true;
            st.last_failure = Some(7);
        }
        Vet.on_user_submit("go on", false);
        let st = state();
        assert_eq!(st.iteration, 0);
        assert!(!st.dirty);
        assert_eq!(st.last_failure, None);
    }

    #[test]
    fn run_guard_is_exclusive_and_released_on_drop() {
        let _g = fresh();
        let first = RunGuard::acquire();
        assert!(first.is_some());
        assert!(RunGuard::acquire().is_none(), "同一时刻只允许一组检查");
        drop(first);
        assert!(RunGuard::acquire().is_some());
    }

    #[test]
    fn check_names_folds_the_tail() {
        let cfg = Config {
            checks: (0..6)
                .map(|i| config::Check {
                    name: format!("c{i}"),
                    command: "true".to_string(),
                    optional: false,
                })
                .collect(),
            ..Config::default()
        };
        let text = check_names(&cfg);
        assert!(text.starts_with("c0, c1, c2, c3"));
        assert!(text.contains("+2 more"));
    }

    #[test]
    fn last_run_label_is_never_before_the_first_run() {
        assert_eq!(last_run_label(None), "never");
        assert!(last_run_label(Some(Instant::now())).ends_with("ago"));
    }

    #[test]
    fn settings_panel_exposes_the_five_switches() {
        let _g = fresh();
        let settings = Vet.settings();
        let keys: Vec<&str> = settings.iter().map(|s| s.key.as_str()).collect();
        assert_eq!(
            keys,
            vec![
                "autoRun",
                "maxIterations",
                "timeout",
                "announce",
                "requireEdits"
            ]
        );
        for setting in &settings {
            assert!(!setting.choices.is_empty(), "面板项都应可循环切换");
        }
    }
}
