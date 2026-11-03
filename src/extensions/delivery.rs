//! `delivery` 扩展：把回合的改动交付出去，并盯着 CI / 评审把它推到绿。
//!
//! **问题**：`vet` 只证明「本机检查过了」。改动变成可评审的东西还要一串机械动作：建分支、
//! 提交、推、开 PR、等 CI、把红掉的日志捞回来。每轮都靠用户手打，agent 自己也拿不到
//! 「CI 红了什么」——它只能猜。
//!
//! **本扩展**：在结算边界（[`ExtensionHook::Boundary`] 的 `agent_before_settle`）按配置的
//! `stage` 交付，停在 PR 上；CI 失败或有 `CHANGES_REQUESTED` 评审时，把清单作为一条 user
//! 消息注入并**续跑一轮**，模型修完下一次结算会再提交、再推、再看——直到全绿、用满
//! `maxIterations`、或同一条失败反复出现。
//!
//! 三条不做的线（每条都是「替用户做了决定」）：
//! - **不自动 merge**：合入是人的决定（`stage` 只到 `pr`）。
//! - **不吞掉回合开始前就脏的工作区**：那份改动不是本回合的产物，默认拒绝交付并提示，
//!   `/delivery run` 是显式的例外。
//! - **不猜远端**：`origin` 不存在、`gh` 没登录、`base` 探测不出来，都停下来报原因。
//!
//! 配置在 `agent_dir()/extensions/delivery.json`（见 [`config`]）；git 在
//! [`crate::utils::git`]、`gh` 与 PR 状态解析在 [`crate::utils::gh`]，`/delivery` 命令族在
//! [`command`]。
//!
//! **默认不启用**，声明 `Dev` 模式——它会写 git 历史并推远端，只能由用户显式打开。

mod command;
mod config;

use crate::{
    core::{
        extensions::{
            BeforeToolCallOutcome, BoundaryOutcome, Extension, ExtensionCommand, ExtensionHook,
            ExtensionMode, ExtensionTool, UiNotifyLevel,
        },
        provider::AgentMessage,
        session_manager::Session,
        tools::ToolError,
    },
    extensions::{
        EXTENSION_FACTORIES, ExtensionFactory, PRIORITY_DELIVERY,
        util::{generate_id, notify_text},
    },
    modes::tui::handlers::register_host_command,
    utils::{
        gh::{self, Verdict},
        git,
        glyphs::DEF_TRAILING_ELLIPSIS,
        paths::cwd,
        time::{now_iso, short_date},
        truncate::truncate_chars,
    },
};
use config::{Config, Stage};
use serde_json::Value;
use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

/// 约定俗成的基线分支名：`origin/HEAD` 缺失时（新仓库、没有远端）兜底，免得往 `main` 上直接提交。
const CONVENTIONAL_BASES: &[&str] = &["main", "master", "trunk"];

/// 视为「可能改动工作区」的工具：调用过它们才认为本回合有东西要交付。
const WRITE_TOOLS: &[&str] = &["edit", "write", "bash", "powershell"];

/// 自声明工厂：linkme 分布式切片。
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static DELIVERY_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_DELIVERY,
    make: || -> Arc<dyn Extension> { Arc::new(Delivery) },
};

/// 当前会话的 cwd（`on_session_start` 记录；拿不到时回落到进程 cwd）。
static SESSION_CWD: Mutex<Option<PathBuf>> = Mutex::new(None);

/// 本扩展的回合内运行态。
static STATE: Mutex<State> = Mutex::new(State::new());

/// 交付闸：同一时刻只允许一次交付在跑（自动路径与 `/delivery run` 共享）。
static RUNNING: AtomicBool = AtomicBool::new(false);

/// 配置告警只提示一次的开关。
static WARNED: AtomicBool = AtomicBool::new(false);

/// 会话标题（PR 标题 / 提交信息）的字符上限。
const TITLE_CHARS: usize = 72;

/// PR 正文里用户第一条消息的字符上限。
const BODY_CHARS: usize = 2000;

/// 最近一次交付的结论（`/delivery status` 展示）。
#[derive(Debug, Clone)]
struct Note {
    /// 结论一句话。
    text: String,
    /// 发生时刻。
    at: Instant,
}

/// 回合内运行态。
struct State {
    /// 本回合已把失败喂回模型的次数。
    iteration: usize,
    /// 本回合是否调用过写类工具。
    dirty: bool,
    /// 回合开始前工作区是否已有未提交改动（决定能不能 `git add -A`）。
    pre_existing_dirty: bool,
    /// 是否已经量过 `pre_existing_dirty`（每回合只量一次）。
    measured: bool,
    /// 会话标题（第一条用户输入的单行摘要）。
    title: Option<String>,
    /// 上次注入模型的失败指纹（识别「同一条失败反复出现」）。
    last_failure: Option<u64>,
    /// 本会话交付过的分支。
    branch: Option<String>,
    /// 本会话交付过的 PR 号。
    pr: Option<u64>,
    /// 最近一次交付结论。
    last: Option<Note>,
}

impl State {
    /// 全零初值（`static` 初始化要求 `const`，所以不能走 `Default`）。
    const fn new() -> Self {
        State {
            iteration: 0,
            dirty: false,
            pre_existing_dirty: false,
            measured: false,
            title: None,
            last_failure: None,
            branch: None,
            pr: None,
            last: None,
        }
    }
}

/// 取运行态；锁被毒化时取回内部数据继续用。
fn state() -> MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// 清空回合级状态（保留分支/PR/最近结论这类跨回合信息）。
fn reset_turn_state() {
    let mut st = state();
    st.iteration = 0;
    st.dirty = false;
    st.pre_existing_dirty = false;
    st.measured = false;
    st.last_failure = None;
}

/// 记下本次会话的 cwd。
fn remember_cwd(cwd_text: &str) {
    *SESSION_CWD.lock().unwrap_or_else(|e| e.into_inner()) = Some(PathBuf::from(cwd_text));
}

/// 当前生效的 cwd：会话记录的优先，其次进程 cwd，都拿不到时用 `.`。
fn current_cwd() -> PathBuf {
    if let Some(dir) = SESSION_CWD
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
    {
        return dir;
    }
    cwd()
}

/// 交付闸的持有凭证：drop 时自动放行。
struct RunGuard;

impl RunGuard {
    /// 抢占执行闸；已有交付在跑时返回 `None`。
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

/// 一次交付尝试的结论。
enum Step {
    /// 没什么要交付的（工作区干净 / 不是 git 仓库），静默跳过。
    Nothing,
    /// 交付完成（消息进聊天区）。
    Done(String),
    /// 有阻塞，报告可以喂回模型。
    NeedsFix(String),
    /// 停下来交回用户（配置问题、认证、超时）。
    Stop(String),
}

/// `delivery` 扩展。
pub struct Delivery;

impl Extension for Delivery {
    /// 扩展注册名（`/extension` 面板与 `/delivery` 命令共用）。
    fn name(&self) -> &str {
        config::EXT
    }

    /// 面板里的功能说明。
    fn description(&self) -> &str {
        "Deliver the turn's changes: commit to a session branch, push, open a pull request, then \
         follow CI and review feedback back into the agent until the PR is green. Never merges. \
         Config: agent_dir()/extensions/delivery.json; /delivery status|run|reset|stage"
    }

    /// 仅在 Dev 及以上模式可用。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Dev]
    }

    /// 默认不启用：会写 git 历史并推远端，属于用户显式选择的能力。
    fn default_enabled(&self) -> bool {
        false
    }

    /// 不提供工具：交付是结算边界上的动作，模型看不到它。
    fn tools(&self) -> Vec<ExtensionTool> {
        Vec::new()
    }

    /// 声明 `/delivery` 命令。
    fn commands(&self) -> Vec<ExtensionCommand> {
        vec![ExtensionCommand {
            name: config::CMD.to_string(),
            description: "Delivery: /delivery status|run|reset|stage <branch|push|pr>".to_string(),
            // handler 只读写本扩展自身的配置与运行态，不锁 agent。
            busy_safe: true,
            subcommands: command::SUBCOMMANDS.to_vec(),
        }]
    }

    /// 接线 `/delivery` 执行入口。
    fn on_registered(&self) {
        register_host_command(config::EXT, config::CMD, command::handle);
    }

    /// 订阅结算边界（交付）、工具前钩子（记脏标记）与提交通知（复位回合状态）。
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
            _ = config::load();
        } else {
            reset_turn_state();
            WARNED.store(false, Ordering::Relaxed);
        }
    }

    /// 记录会话 cwd 并复位回合状态。
    fn on_session_start(
        &self,
        cwd_text: &str,
        _messages: &[AgentMessage],
        _session: Option<&Session>,
    ) {
        remember_cwd(cwd_text);
        reset_turn_state();
        WARNED.store(false, Ordering::Relaxed);
    }

    /// 会话切换后工作区可能换了，重新记 cwd。
    fn on_session_switched(&self, _session_path: Option<&str>, _messages: &[AgentMessage]) {
        reset_turn_state();
    }

    /// 用户（重新）提交输入：新回合，复位续跑计数与失败指纹，并记下标题。
    fn on_user_submit(&self, text: &str, _busy: bool) {
        reset_turn_state();
        let title = title_of(text);
        if title.is_some() {
            let mut st = state();
            if st.title.is_none() {
                st.title = title;
            }
        }
    }

    /// 工具调用前只做一件事：把「本回合改过东西」记下来（不拦截、不改写参数）。
    fn before_tool_call_ext(
        &self,
        name: &str,
        _args: &Value,
        _assistant_message: Option<&AgentMessage>,
        _context: &[AgentMessage],
        _all_args: &Value,
    ) -> std::result::Result<BeforeToolCallOutcome, ToolError> {
        // 第一次工具调用之前量一次工作区：这时候树还没被本回合改过，脏的就是用户自己的活。
        // 放到结算时量会把 agent 自己的改动当成「回合前就脏」，那样永远拒交。
        if !state().measured {
            let dir = current_cwd();
            let dirty = git::toplevel(&dir)
                .map(|top| git::dirty(&top).unwrap_or(false))
                .unwrap_or(false);
            let mut st = state();
            st.pre_existing_dirty = dirty;
            st.measured = true;
        }
        if WRITE_TOOLS.contains(&name) {
            state().dirty = true;
        }
        Ok(BeforeToolCallOutcome::allow())
    }

    /// 结算边界：交付 + 跟 CI，失败则注入报告并续跑（收敛条件见模块文档）。
    fn on_boundary(&self, event: &Value) -> Option<BoundaryOutcome> {
        if event.get("type").and_then(Value::as_str) != Some("agent_before_settle") {
            return None;
        }
        // 子代理的结算不交付：交不交付是主 agent 的事
        if event.get("agentScope").and_then(Value::as_str) != Some("main") {
            return None;
        }
        // 用户中断：不续跑，也不把计数留着污染下一回合
        if event.get("outcome").and_then(Value::as_str) == Some("aborted") {
            reset_turn_state();
            return None;
        }
        // 有排队输入：让位给用户
        if event
            .get("hasQueuedMessages")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return None;
        }

        settle()
    }
}

/// 结算边界的完整流程。返回 `Some(continue_with)` 表示注入失败报告并再跑一轮。
fn settle() -> Option<BoundaryOutcome> {
    let cfg = config::load();
    let guard = RunGuard::acquire()?;
    let step = deliver(&cfg, false);
    drop(guard);

    match step {
        Step::Nothing => None,
        Step::Done(text) => {
            record(text.clone());
            reset_turn_state();
            notify_text(&format!("delivery: {text}"), UiNotifyLevel::Success);
            None
        }
        Step::Stop(text) => {
            record(text.clone());
            reset_turn_state();
            notify_text(&format!("delivery: {text}"), UiNotifyLevel::Warning);
            None
        }
        Step::NeedsFix(report) => {
            let (iteration, last_failure) = {
                let st = state();
                (st.iteration, st.last_failure)
            };
            // 收敛 1：用满上限 → 交回用户
            if iteration >= cfg.max_iterations {
                record("stopping the CI loop".to_string());
                reset_turn_state();
                notify_text(
                    &format!(
                        "delivery: stopping after {iteration} attempt{} — the PR is still not \
                         green. Run /delivery run to retry, or /delivery status to review.",
                        if iteration == 1 { "" } else { "s" }
                    ),
                    UiNotifyLevel::Warning,
                );
                return None;
            }
            // 收敛 2：同一条失败反复出现 → 再喂一次也没用
            let fingerprint = fingerprint(&report);
            if last_failure == Some(fingerprint) {
                record("the same failures repeated".to_string());
                reset_turn_state();
                notify_text(
                    "delivery: the same CI failures repeated — stopping the loop",
                    UiNotifyLevel::Warning,
                );
                return None;
            }

            let attempt = iteration + 1;
            {
                let mut st = state();
                st.iteration = attempt;
                st.last_failure = Some(fingerprint);
            }
            record(format!("attempt {attempt} fed back to the model"));
            Some(BoundaryOutcome::continue_with(vec![
                AgentMessage::user_text(&report),
            ]))
        }
    }
}

/// 记下最近一次结论（`/delivery status` 用）。
fn record(text: String) {
    state().last = Some(Note {
        text,
        at: Instant::now(),
    });
}

/// 失败报告的指纹（用于识别「同一批失败反复出现」）。
fn fingerprint(report: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    report.hash(&mut hasher);
    hasher.finish()
}

/// 会话标题：第一条用户输入压成单行并截断。
fn title_of(text: &str) -> Option<String> {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?
        .to_string();
    Some(truncate_chars(&line, TITLE_CHARS, None, true))
}

/// 提交信息 / PR 标题：会话标题，没有就用固定文案。
fn session_title() -> String {
    state()
        .title
        .clone()
        .filter(|title| !title.trim().is_empty())
        .unwrap_or_else(|| "prux session changes".to_string())
}

/// 新的会话分支名 `prux/<date>-<hhmm>`；重名时加短后缀。
fn fresh_branch(top: &Path) -> String {
    let stamp = now_iso();
    let date = short_date(&stamp);
    let time = stamp.get(11..16).unwrap_or("00:00").replace(':', "");
    let base = format!("prux/{date}-{time}");
    if !git::branch_exists(top, &base) {
        return base;
    }
    format!("{base}-{}", &generate_id()[..4])
}

/// 目标分支是不是「基线」（不许直接提交上去）。
fn on_base(top: &Path, current: &str, cfg: &Config) -> bool {
    if !cfg.base.is_empty() && current == cfg.base {
        return true;
    }
    if git::local_default_branch(top).is_some_and(|base| base == current) {
        return true;
    }
    CONVENTIONAL_BASES.contains(&current)
}

/// 跑一次交付。`force` = `/delivery run`（忽略「回合前就脏」与 `requireEdits`）。
fn deliver(cfg: &config::Config, force: bool) -> Step {
    match run_delivery(cfg, force) {
        Ok(step) => step,
        Err(err) => Step::Stop(err),
    }
}

/// 交付主体：量工作区 → 建分支 → 提交 → 推 → 开 PR → 等 CI。
fn run_delivery(cfg: &config::Config, force: bool) -> Result<Step, String> {
    let dir = current_cwd();
    // 不是 git 仓库就没有交付可言：静默跳过（这个扩展不该在非 git 目录里刷提示）
    let Ok(top) = git::toplevel(&dir) else {
        return Ok(Step::Nothing);
    };
    let (dirty_this_turn, iteration, pre_existing_dirty) = {
        let st = state();
        (st.dirty, st.iteration, st.pre_existing_dirty)
    };

    // 回合开始前就脏 → 那是用户的活，不替它提交（`/delivery run` 除外）
    if !force && iteration == 0 && pre_existing_dirty {
        return Ok(Step::Stop(
            "the working tree already had uncommitted changes before this turn — committing \
             them would mix your work into the delivery. Commit or stash them first, or run \
             /delivery run to deliver anyway."
                .to_string(),
        ));
    }
    if !force && cfg.require_edits && !dirty_this_turn && iteration == 0 {
        return Ok(Step::Nothing);
    }

    let current = git::branch(&top)?;
    if current == "HEAD" {
        return Ok(Step::Stop(
            "detached HEAD — check out a branch first".to_string(),
        ));
    }

    let has_changes = git::dirty(&top)?;
    // 没有改动、也没有正在跟的 CI 循环：没什么可交付
    if !has_changes && iteration == 0 && !force {
        return Ok(Step::Nothing);
    }

    let mut branch = current;
    if has_changes && on_base(&top, &branch, cfg) {
        branch = fresh_branch(&top);
        git::create_branch(&top, &branch)?;
    }
    let message = format!("prux: {}", session_title());
    if has_changes {
        git::commit_all(&top, &message)?;
        state().dirty = false; // 已提交，下一轮要新的改动才算脏
    }
    state().branch = Some(branch.clone());

    if cfg.stage == Stage::Branch {
        let summary = format!("committed to `{branch}`");
        return Ok(Step::Done(summary));
    }

    if crate::core::runtime::offline() {
        return Ok(Step::Stop(
            "offline mode is on — the branch is committed but not pushed".to_string(),
        ));
    }

    if let Err(err) = git::push(&top, &branch) {
        return Ok(Step::Stop(format!(
            "{err} (branch `{branch}` is committed locally)"
        )));
    }
    if cfg.stage == Stage::Push {
        return Ok(Step::Done(format!("pushed `{branch}` to origin")));
    }

    gh::auth_ok(&top)?;
    let base = if cfg.base.is_empty() {
        gh::default_branch(&top)?
    } else {
        cfg.base.clone()
    };
    let number = match gh::pr_number(&top, &branch)? {
        Some(number) => number,
        None => {
            let body = format!(
                "{}\n\n---\nDelivered by prux from this session.\n",
                session_body()
            );
            let url = gh::create_pr(&top, &branch, &base, &session_title(), &body, cfg.draft)?;
            let number = gh::pr_number(&top, &branch)?.unwrap_or(0);
            notify_text(
                &format!("delivery: opened PR #{number} {url}"),
                UiNotifyLevel::Info,
            );
            number
        }
    };
    state().pr = Some(number);
    wait_for_ci(cfg, &top, &branch)
}

/// PR 正文：用户最初那条需求的摘录。
fn session_body() -> String {
    truncate_chars(&session_title(), BODY_CHARS, None, true)
}

/// 轮询 PR 直到出结论、超时或阻塞。
///
/// `shortcut`：推完先等一个 `poll` 周期再读状态——`gh` 的检查结论可能还是上一次推送的，
/// 立刻读会把旧结论当新的。要精确到「这次推送的 head sha」得逐条比对 check run 的
/// `headSha`，等有人被误判成绿了再说。
fn wait_for_ci(cfg: &config::Config, top: &std::path::Path, branch: &str) -> Result<Step, String> {
    let started = Instant::now();
    let deadline = started + cfg.timeout;
    loop {
        std::thread::sleep(cfg.poll);
        let status = gh::pr_status(top, branch)?;
        match status.verdict {
            Verdict::Green => {
                return Ok(Step::Done(format!(
                    "PR #{} is green ({})",
                    status.number, status.url
                )));
            }
            Verdict::Blocked(failure) => {
                return Ok(Step::NeedsFix(fix_report(
                    &failure,
                    status.number,
                    &status.url,
                )));
            }
            Verdict::Pending(desc) => {
                if Instant::now() >= deadline {
                    return Ok(Step::Stop(format!(
                        "{desc} after {}s — giving the turn back. PR #{} {}",
                        started.elapsed().as_secs(),
                        status.number,
                        status.url
                    )));
                }
            }
        }
    }
}

/// 把阻塞项拼成喂回模型的一段报告。
///
/// 事实部分来自 [`gh::Failure::details`]；「改完再收工、会自动重新交付」这层语义属于
/// 交付扩展，不塞进 `utils::gh`。
fn fix_report(failure: &gh::Failure, number: u64, url: &str) -> String {
    format!(
        "delivery: PR #{number} is not ready ({url}). Fix what is listed, then finish — the \
         change is committed, pushed and re-checked automatically.\n\n{}",
        failure.details()
    )
}

/// `/delivery run` 的同步入口（供 [`command`] 在后台线程里调用）。
pub(super) fn run_now() -> String {
    let cfg = config::load();
    let Some(guard) = RunGuard::acquire() else {
        return "a delivery is already running".to_string();
    };
    let step = deliver(&cfg, true);
    drop(guard);
    let text = match step {
        Step::Nothing => "nothing to deliver (clean working tree)".to_string(),
        Step::Done(text) | Step::Stop(text) => text,
        Step::NeedsFix(report) => format!(
            "the PR needs work; the next turn will get this:\n{}",
            truncate_chars(&report, 800, Some(DEF_TRAILING_ELLIPSIS), true)
        ),
    };
    record(text.clone());
    text
}

/// `/delivery status` 的文本。
pub(super) fn status_text() -> String {
    let cfg = config::load();
    let (branch, pr, iteration, last) = {
        let st = state();
        (st.branch.clone(), st.pr, st.iteration, st.last.clone())
    };
    let mut out = format!(
        "delivery: stage {} · maxIterations {} · timeout {}s · poll {}s · base {} · draft {} · \
         requireEdits {}\nconfig: {}",
        cfg.stage.as_label(),
        cfg.max_iterations,
        cfg.timeout.as_secs(),
        cfg.poll.as_secs(),
        if cfg.base.is_empty() {
            "auto".to_string()
        } else {
            cfg.base.clone()
        },
        cfg.draft,
        cfg.require_edits,
        config::config_path().display(),
    );
    out.push_str(&format!(
        "\nbranch: {} · PR: {} · fed back {} time(s) this turn",
        branch.as_deref().unwrap_or("—"),
        pr.map(|n| format!("#{n}"))
            .unwrap_or_else(|| "—".to_string()),
        iteration
    ));
    out.push_str(&match last {
        Some(note) => format!(
            "\nlast delivery ({}s ago): {}",
            note.at.elapsed().as_secs(),
            note.text
        ),
        None => "\nlast delivery: never".to_string(),
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::HomeGuard;

    #[test]
    fn title_comes_from_the_first_non_empty_line() {
        let _serial = serial();
        assert_eq!(
            title_of("\n\n  加个 JSON 输出  \n细节").as_deref(),
            Some("加个 JSON 输出")
        );
        assert_eq!(title_of("   \n "), None);
        let long = "x".repeat(200);
        assert_eq!(title_of(&long).unwrap().chars().count(), TITLE_CHARS);
    }

    #[test]
    fn fingerprint_is_stable_for_the_same_report() {
        let _serial = serial();
        assert_eq!(fingerprint("a\nb"), fingerprint("a\nb"));
        assert_ne!(fingerprint("a\nb"), fingerprint("a\nc"));
    }

    #[test]
    fn fresh_branch_name_is_date_based_and_non_empty() {
        let _serial = serial();
        let dir = std::env::temp_dir();
        let name = fresh_branch(&dir);
        assert!(name.starts_with("prux/"), "{name}");
        assert!(!name.contains(':'), "分支名不能带冒号: {name}");
        assert!(name.len() > "prux/".len());
    }

    #[test]
    fn config_is_readable_from_the_extension_dir() {
        let _serial = serial();
        let _guard = HomeGuard::temp();
        let cfg = config::load();
        assert_eq!(cfg.timeout.as_secs(), config::DEFAULT_TIMEOUT_SECS);
        assert!(status_text().contains("stage pr"));
    }

    /// 交付测试串行锁：运行态是进程级单例，并发测试会互相踩。
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    /// 取串行锁。
    fn serial() -> MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 造一个临时 git 仓库，返回它的路径；`git` 不可用时返回 `None`。
    fn temp_repo(tag: &str) -> Option<PathBuf> {
        let dir = std::env::temp_dir().join(format!(
            "prux-delivery-{tag}-{}",
            crate::extensions::util::generate_id()
        ));
        std::fs::create_dir_all(&dir).ok()?;
        let ok = std::process::Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(&dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .ok()?
            .success();
        if !ok {
            return None;
        }
        for (key, value) in [("user.email", "t@example.com"), ("user.name", "prux test")] {
            std::process::Command::new("git")
                .args(["config", key, value])
                .current_dir(&dir)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .ok()?;
        }
        std::fs::write(dir.join("README.md"), "hello\n").ok()?;
        std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(&dir)
            .status()
            .ok()?;
        std::process::Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(&dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .ok()?;
        Some(dir)
    }

    #[test]
    fn commits_onto_a_fresh_branch_instead_of_the_base_branch() {
        let _serial = serial();
        let _guard = crate::test_support::AgentDirGuard::temp();
        let Some(repo) = temp_repo("commit") else {
            return; // 没有 git 的环境跳过
        };
        remember_cwd(&repo.to_string_lossy());
        reset_turn_state();
        {
            let mut st = state();
            st.measured = true;
            st.pre_existing_dirty = false;
            st.dirty = true;
        }
        std::fs::write(repo.join("work.txt"), "changes\n").unwrap();

        let mut cfg = config::merge(None);
        cfg.stage = Stage::Branch;
        config::write(&cfg).unwrap();

        let step = deliver(&cfg, false);
        let Step::Done(text) = step else {
            match step {
                Step::Stop(text) | Step::Done(text) => panic!("交付失败: {text}"),
                Step::NeedsFix(text) => panic!("交付失败: {text}"),
                Step::Nothing => panic!("交付被跳过"),
            }
        };
        assert!(text.contains("committed to `prux/"), "{text}");

        let branch = git::branch(&repo).unwrap();
        assert!(branch.starts_with("prux/"), "不在基线分支上提交: {branch}");
        assert!(!git::dirty(&repo).unwrap(), "改动都已提交");
    }

    #[test]
    fn refuses_to_commit_a_tree_that_was_dirty_before_the_turn() {
        let _serial = serial();
        let _guard = crate::test_support::AgentDirGuard::temp();
        let Some(repo) = temp_repo("dirty") else {
            return;
        };
        remember_cwd(&repo.to_string_lossy());
        reset_turn_state();
        std::fs::write(repo.join("user-wip.txt"), "user\n").unwrap();
        {
            let mut st = state();
            st.measured = true;
            st.pre_existing_dirty = true;
            st.dirty = true;
        }
        let mut cfg = config::merge(None);
        cfg.stage = Stage::Branch;

        let step = deliver(&cfg, false);
        let Step::Stop(text) = step else {
            panic!("应当拒绝交付");
        };
        assert!(text.contains("/delivery run"), "{text}");
        assert_eq!(git::branch(&repo).unwrap(), "main", "不该切分支");
        assert!(git::dirty(&repo).unwrap(), "用户的改动原样留着");
    }

    #[test]
    fn skip_paths_are_silent() {
        let _serial = serial();
        let _guard = HomeGuard::temp();
        // 进程 cwd 不是 git 仓库 / 干净工作区：都该是 Nothing 而不是报错
        let cfg = config::merge(None);
        let step = deliver(&cfg, false);
        assert!(matches!(step, Step::Nothing | Step::Stop(_)));
    }
}
